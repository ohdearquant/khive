//! KhiveRuntime — composable handle to all storage capabilities.

use std::sync::{Arc, RwLock};

use khive_db::StorageBackend;
use khive_gate::{ActorRef, AllowAllGate, GateRef, GateRequest};
use khive_storage::{EntityStore, EventStore, GraphStore, NoteStore, SqlAccess};
use khive_types::{EdgeEndpointRule, Namespace};
use lattice_embed::{EmbeddingModel, EmbeddingService};

use crate::error::RuntimeResult;

// ---- BackendId ----

/// Identifies a named backend in a multi-backend deployment.
///
/// The `main` backend is the default single-backend name. Multi-backend deployments
/// assign each `[[backends]]` entry a distinct `BackendId`. The
/// [`SubstrateCoordinator`](kkernel::coordinator::SubstrateCoordinator) in `kkernel`
/// uses `BackendId` for node-to-backend resolution and cross-backend edge routing.
///
/// A single-backend `KhiveRuntime` always has `BackendId("main")` by default.
/// The boot path in `kkernel` or `khive-mcp` sets the id via `RuntimeConfig::backend_id`
/// when constructing per-pack runtimes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendId(pub String);

impl BackendId {
    /// The default single-backend name.
    pub const MAIN: &'static str = "main";

    /// Construct from a string name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The default `main` backend id.
    pub fn main() -> Self {
        Self(Self::MAIN.to_string())
    }

    /// Return the backend name as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---- Sealed token ----

mod private {
    #[derive(Clone, Debug)]
    pub(crate) struct Sealed;
}

/// Authorization proof that a caller is permitted to access a specific namespace.
///
/// Created by [`VerbRegistry::dispatch`] after the gate approves the request.
/// The sealed inner field prevents external code from constructing a token
/// without going through the authorization path.
#[derive(Clone, Debug)]
pub struct NamespaceToken {
    namespace: Namespace,
    actor: ActorRef,
    _sealed: private::Sealed,
}

impl NamespaceToken {
    /// Mint an authorized token. Only callable from within `khive-runtime`.
    pub(crate) fn mint_authorized(namespace: Namespace, actor: ActorRef) -> Self {
        Self {
            namespace,
            actor,
            _sealed: private::Sealed,
        }
    }

    /// Convenience constructor for the local namespace with an anonymous actor.
    ///
    /// Only callable from within `khive-runtime`. External callers must use
    /// [`KhiveRuntime::authorize`] to mint tokens.
    // Used only in #[cfg(test)] blocks within this crate's src/ files.
    #[allow(dead_code)]
    pub(crate) fn local() -> Self {
        Self::mint_authorized(Namespace::local(), ActorRef::anonymous())
    }

    /// Convenience constructor for a specific namespace with an anonymous actor.
    ///
    /// Only callable from within `khive-runtime`. External callers must use
    /// [`KhiveRuntime::authorize`] to mint tokens.
    // Used only in #[cfg(test)] blocks within this crate's src/ files.
    #[allow(dead_code)]
    pub(crate) fn for_namespace(ns: Namespace) -> Self {
        Self::mint_authorized(ns, ActorRef::anonymous())
    }

    /// Return the namespace this token authorises access to.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// Return the actor reference embedded in this token.
    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    /// Return a new token with the same actor but a different namespace.
    ///
    /// Used by packs that apply a namespace policy (e.g. the KG pack overrides the
    /// caller's namespace to `Namespace::local()` so that entity/edge/note records
    /// always land in the shared graph).
    pub fn with_namespace(&self, ns: Namespace) -> Self {
        Self::mint_authorized(ns, self.actor.clone())
    }
}

// ---- RuntimeConfig ----

/// Runtime configuration.
///
/// The `db_path` and `embedding_model` fields are deprecated in favour of
/// constructing the backend externally and calling [`KhiveRuntime::from_backend`].
/// They remain for backward compatibility with tests and single-binary deployments.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Path to the SQLite database file. `None` = in-memory (tests).
    ///
    /// Deprecated: use [`KhiveRuntime::from_backend`] instead. The boot path
    /// constructs backends from `khive.toml` (`AppConfig`) and passes them to
    /// `from_backend`. Direct `db_path` usage persists only in tests.
    pub db_path: Option<std::path::PathBuf>,
    /// Namespace used when no explicit namespace is provided.
    pub default_namespace: Namespace,
    /// Local embedding model. `None` disables embedding and hybrid vector search;
    /// `hybrid_search` then falls back to text-only.
    ///
    /// Deprecated: embedding engines move to a per-pack `EmbedderRegistry`.
    /// This field persists for backward compatibility until the embedder registry
    /// is fully plumbed.
    pub embedding_model: Option<EmbeddingModel>,
    /// Additional embedding models to make available by request name.
    ///
    /// `embedding_model` remains the default used by existing `embed()` and
    /// `embed_batch()` callers. This list adds non-default models that can be
    /// selected with `embedder(name)`, `embed_with_model(...)`, memory
    /// `remember.embedding_model`, and memory `recall.embedding_model`.
    pub additional_embedding_models: Vec<EmbeddingModel>,
    /// Authorization gate consulted before each verb dispatch.
    /// Default: `AllowAllGate` (permissive). For production policy enforcement,
    /// plug in a Rego- or capability-witness-backed impl.
    pub gate: GateRef,
    /// Names of packs the transport layer should register into the VerbRegistry.
    /// The transport layer (e.g. `khive-mcp`) reads this list and instantiates
    /// the matching concrete pack types. Unknown names are reported as errors
    /// by the transport, not silently ignored.
    /// Default: `["kg"]`.
    pub packs: Vec<String>,
    /// Identifies this runtime's backend in a multi-backend deployment.
    ///
    /// Set by the boot path when constructing per-pack runtimes from `khive.toml`.
    /// Single-backend deployments use the default `BackendId::MAIN`.
    pub backend_id: BackendId,
}

/// Parse a comma- or whitespace-separated pack list from a single string.
///
/// Empty entries are dropped, surrounding whitespace is trimmed.
pub fn parse_pack_list(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let db_path = std::env::var("HOME")
            .ok()
            .map(|h| std::path::PathBuf::from(h).join(".khive/khive-graph.db"));
        let embedding_model = std::env::var("KHIVE_EMBEDDING_MODEL")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(Some(EmbeddingModel::AllMiniLmL6V2));
        let additional_embedding_models = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
            .ok()
            .map(|s| parse_embedding_model_list(&s))
            .unwrap_or_else(|| vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2]);
        let packs = std::env::var("KHIVE_PACKS")
            .ok()
            .map(|s| parse_pack_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                vec![
                    "kg",
                    "gtd",
                    "memory",
                    "brain",
                    "comm",
                    "schedule",
                    "knowledge",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            });
        Self {
            db_path,
            default_namespace: Namespace::local(),
            embedding_model,
            additional_embedding_models,
            gate: Arc::new(AllowAllGate),
            packs,
            backend_id: BackendId::main(),
        }
    }
}

// ---- KhiveRuntime ----

/// Composable runtime handle used by the MCP server.
///
/// Wraps a `StorageBackend` and provides namespace-scoped accessor methods
/// for each storage capability, plus a lazily-loaded embedder.
#[derive(Clone)]
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    config: RuntimeConfig,
    /// Pack-extensible embedder registry.
    ///
    /// Shared across clones via `Arc<RwLock<_>>` so that
    /// [`register_embedder`](Self::register_embedder) after clone is visible
    /// to all handles. Built-in lattice models are pre-registered during
    /// construction; packs may add more via [`PackRuntime::register_embedders`].
    embedder_registry: Arc<std::sync::RwLock<crate::embedder_registry::EmbedderRegistry>>,
    default_embedder_name: Arc<str>,
    /// Pack-extensible edge endpoint rules. Shared across clones
    /// via `Arc<RwLock<_>>`; installed once by the transport after the
    /// `VerbRegistry` is built. Empty until installed
    edge_rules: Arc<RwLock<Vec<EdgeEndpointRule>>>,
    /// Pack-aggregated valid entity and note kind strings.
    ///
    /// Installed by the transport layer after building the `VerbRegistry`.
    /// When non-empty, `create_entity`, `create_note_inner`, and `import_kg`
    /// reject kinds not in these sets. When empty (no packs loaded, e.g.
    /// bare runtime in unit tests), kind validation is skipped — the pack
    /// handler layer is the primary enforcement point.
    valid_entity_kinds: Arc<RwLock<Vec<String>>>,
    valid_note_kinds: Arc<RwLock<Vec<String>>>,
}

impl KhiveRuntime {
    /// Create a new runtime with the given config.
    ///
    /// The config's `db_path` is used to open or create the SQLite backend.
    /// For the preferred boot path in multi-backend deployments, use
    /// [`from_backend`](Self::from_backend) instead.
    pub fn new(config: RuntimeConfig) -> RuntimeResult<Self> {
        let backend = match &config.db_path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                StorageBackend::sqlite(path)?
            }
            None => StorageBackend::memory()?,
        };
        // Run versioned migrations (V1..V17) at startup so file-backed and
        // in-memory DBs both have proposals_open (V15) and the embedding_model
        // columns (V16/V17) before any pack handler runs.  Migration is
        // idempotent — already-applied versions are skipped.  A failure here
        // aborts construction so the caller sees a clear error rather than a
        // cryptic "no such table" on the first verb dispatch.
        {
            let mut writer = backend.pool().try_writer()?;
            khive_db::run_migrations(writer.conn_mut())?;
        }
        register_configured_embedding_models(&backend, &config)?;
        let (registry, default_embedder_name) = build_embedder_registry(&config);
        Ok(Self {
            backend: Arc::new(backend),
            config,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Open a runtime for read-only inspection (no model registration, no DB creation).
    ///
    /// Runs migrations (idempotent) but skips `register_configured_embedding_models`,
    /// so `engine list` / `engine status` cannot mutate the registry as a side effect.
    /// Returns `None` when `db_path` is `None` and the default DB does not exist.
    pub fn new_readonly(config: RuntimeConfig) -> RuntimeResult<Self> {
        let backend = match &config.db_path {
            Some(path) => StorageBackend::sqlite(path)?,
            None => StorageBackend::memory()?,
        };
        {
            let mut writer = backend.pool().try_writer()?;
            khive_db::run_migrations(writer.conn_mut())?;
        }
        let (registry, default_embedder_name) = build_embedder_registry(&config);
        Ok(Self {
            backend: Arc::new(backend),
            config,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Construct a runtime from an already-opened backend.
    ///
    /// This is the preferred constructor for multi-backend deployments. The caller
    /// (boot path in `kkernel` or `khive-mcp`) opens each backend from `khive.toml`,
    /// then constructs a `KhiveRuntime` per pack using this method.
    ///
    /// The returned runtime has `db_path = None` and `embedding_model = None`; all
    /// storage access is through the provided `backend`. Set `backend_id` and
    /// `default_namespace` via the config builder pattern if non-defaults are needed.
    pub fn from_backend(backend: Arc<StorageBackend>, config: RuntimeConfig) -> Self {
        if let Err(err) = register_configured_embedding_models(&backend, &config) {
            tracing::warn!(error = %err, "failed to register configured embedding models");
        }
        let (registry, default_embedder_name) = build_embedder_registry(&config);
        Self {
            backend,
            config,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Create an in-memory runtime (for tests and ephemeral use).
    pub fn memory() -> RuntimeResult<Self> {
        Self::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        })
    }

    /// Return the [`BackendId`] for this runtime's backend.
    ///
    /// Used by the [`SubstrateCoordinator`](kkernel::coordinator::SubstrateCoordinator)
    /// to identify which backend owns a given node, and to detect cross-backend merges.
    pub fn backend_id(&self) -> &BackendId {
        &self.config.backend_id
    }

    /// Return a reference to the runtime config.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Return a reference to the underlying storage backend.
    pub fn backend(&self) -> &StorageBackend {
        &self.backend
    }

    // ---- Store accessors (token-scoped) ----

    /// Get an EntityStore scoped to the token's namespace.
    pub fn entities(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn EntityStore>> {
        Ok(self
            .backend
            .entities_for_namespace(token.namespace().as_str())?)
    }

    /// Get a GraphStore scoped to the token's namespace.
    pub fn graph(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn GraphStore>> {
        Ok(self
            .backend
            .graph_for_namespace(token.namespace().as_str())?)
    }

    /// Get a NoteStore scoped to the token's namespace.
    pub fn notes(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn NoteStore>> {
        Ok(self
            .backend
            .notes_for_namespace(token.namespace().as_str())?)
    }

    /// Get an EventStore scoped to the token's namespace.
    pub fn events(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn EventStore>> {
        Ok(self
            .backend
            .events_for_namespace(token.namespace().as_str())?)
    }

    /// Get the raw SQL access capability (for ad-hoc queries).
    pub fn sql(&self) -> Arc<dyn SqlAccess> {
        self.backend.sql()
    }

    /// Get a VectorStore for the configured embedding model, scoped to the token's namespace.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is set.
    pub fn vectors(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        let model = self.resolve_embedding_model(None)?;
        self.vectors_for_embedding_model(token, model)
    }

    /// Get a VectorStore for a specific named embedding model, scoped to the token's namespace.
    ///
    /// Accepts both built-in lattice model names/aliases and custom provider names
    /// registered via [`register_embedder`](Self::register_embedder). Lattice names
    /// are routed through the enum-backed path; custom provider names use the
    /// provider's declared `dimensions()` directly so that the vector store key
    /// is consistent with how vectors were written during `remember`/`recall`.
    pub fn vectors_for_model(
        &self,
        token: &NamespaceToken,
        model_name: &str,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        // Try the lattice enum path first (handles aliases like "paraphrase").
        if let Some(model) = parse_embedding_model_alias(model_name) {
            // Only proceed via the lattice path if this model is actually in the
            // registry; otherwise fall through to the custom-provider path.
            let key = model.to_string();
            let in_registry = self
                .embedder_registry
                .read()
                .map(|reg| reg.contains(&key))
                .unwrap_or(false);
            if in_registry {
                return self.vectors_for_embedding_model(token, model);
            }
        }
        // Custom provider path: look up dimensions from the registry and build
        // the vector store using the sanitized provider name as the table key.
        let dims = {
            let registry = self.embedder_registry.read().map_err(|_| {
                crate::RuntimeError::Internal("embedder registry lock poisoned".into())
            })?;
            registry
                .get_provider(model_name)
                .map(|p| p.dimensions())
                .ok_or_else(|| crate::RuntimeError::UnknownModel(model_name.to_string()))?
        };
        let model_key = sanitize_key(model_name);
        Ok(self.backend.vectors_for_namespace(
            &model_key,
            model_name,
            dims,
            token.namespace().as_str(),
        )?)
    }

    fn vectors_for_embedding_model(
        &self,
        token: &NamespaceToken,
        model: EmbeddingModel,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        Ok(self.backend.vectors_for_namespace(
            &vec_model_key(model),
            &model.to_string(),
            model.dimensions(),
            token.namespace().as_str(),
        )?)
    }

    /// Get a TextSearch index for the token's namespace entity corpus.
    pub fn text(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let key = format!("entities_{}", sanitize_key(token.namespace().as_str()));
        Ok(self.backend.text(&key)?)
    }

    /// Get a TextSearch index for the token's namespace notes corpus.
    pub fn text_for_notes(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let key = format!("notes_{}", sanitize_key(token.namespace().as_str()));
        Ok(self.backend.text(&key)?)
    }

    /// Mint an authorization token for the given namespace.
    ///
    /// Consults the configured [`Gate`] before minting. With the default
    /// `AllowAllGate` this always succeeds. When a real policy-backed gate is
    /// installed, this method enforces it and returns `PermissionDenied` on
    /// denial.
    pub fn authorize(&self, ns: Namespace) -> RuntimeResult<NamespaceToken> {
        let actor = ActorRef::anonymous();
        let req = GateRequest::new(
            actor.clone(),
            ns.clone(),
            "authorize",
            serde_json::Value::Null,
        );
        match self.config.gate.check(&req) {
            Ok(ref decision) if decision.is_allow() => {
                if let khive_gate::GateDecision::Allow { ref obligations } = decision {
                    if !obligations.is_empty() {
                        tracing::debug!(
                            namespace = %ns.as_str(),
                            "authorize: obligations={:?}",
                            obligations
                        );
                    }
                }
                Ok(NamespaceToken::mint_authorized(ns, actor))
            }
            Ok(khive_gate::GateDecision::Deny { reason }) => {
                Err(crate::RuntimeError::PermissionDenied {
                    verb: "authorize".to_string(),
                    reason,
                })
            }
            Ok(_) => Err(crate::RuntimeError::PermissionDenied {
                verb: "authorize".to_string(),
                reason: "gate denied".to_string(),
            }),
            Err(e) => Err(crate::RuntimeError::Internal(format!("gate error: {e}"))),
        }
    }

    /// Install the pack-aggregated edge endpoint rules.
    ///
    /// Called by the transport layer after the `VerbRegistry` is built so
    /// that runtime-layer edge validation can consult pack rules. Idempotent:
    /// later calls overwrite the previous rule set.
    pub fn install_edge_rules(&self, rules: Vec<EdgeEndpointRule>) {
        if let Ok(mut guard) = self.edge_rules.write() {
            *guard = rules;
        }
    }

    /// Install the pack-aggregated valid entity and note kinds.
    ///
    /// Called by the transport layer after the `VerbRegistry` is built so that
    /// runtime-layer entity/note creation and import validate kind strings against
    /// the merged pack vocabulary. Idempotent: later calls overwrite previous sets.
    ///
    /// When no kinds are installed (empty lists), kind validation is skipped at
    /// the runtime layer. The pack handler layer remains the primary enforcement
    /// point; this provides defense-in-depth for direct Rust callers and import.
    pub fn install_kind_registry(&self, entity_kinds: Vec<String>, note_kinds: Vec<String>) {
        if let Ok(mut guard) = self.valid_entity_kinds.write() {
            *guard = entity_kinds;
        }
        if let Ok(mut guard) = self.valid_note_kinds.write() {
            *guard = note_kinds;
        }
    }

    /// Validate that `kind` is a pack-registered entity kind.
    ///
    /// Returns `Ok(())` when no kinds are installed (bare runtime without packs).
    /// Returns `InvalidInput` when kinds are installed and `kind` is not among them.
    pub(crate) fn validate_entity_kind(&self, kind: &str) -> crate::RuntimeResult<()> {
        let guard = self.valid_entity_kinds.read().map_err(|_| {
            crate::RuntimeError::Internal("entity kind registry lock poisoned".into())
        })?;
        if guard.is_empty() {
            return Ok(());
        }
        if guard.iter().any(|k| k == kind) {
            Ok(())
        } else {
            Err(crate::RuntimeError::InvalidInput(format!(
                "unknown entity kind {kind:?}; valid: {}",
                guard.join(", ")
            )))
        }
    }

    /// Validate that `kind` is a pack-registered note kind.
    ///
    /// Returns `Ok(())` when no kinds are installed (bare runtime without packs).
    /// Returns `InvalidInput` when kinds are installed and `kind` is not among them.
    pub(crate) fn validate_note_kind(&self, kind: &str) -> crate::RuntimeResult<()> {
        let guard = self.valid_note_kinds.read().map_err(|_| {
            crate::RuntimeError::Internal("note kind registry lock poisoned".into())
        })?;
        if guard.is_empty() {
            return Ok(());
        }
        if guard.iter().any(|k| k == kind) {
            Ok(())
        } else {
            Err(crate::RuntimeError::InvalidInput(format!(
                "unknown note kind {kind:?}; valid: {}",
                guard.join(", ")
            )))
        }
    }

    /// Snapshot of currently-installed pack edge rules.
    pub(crate) fn pack_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.edge_rules
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Return the name of the default embedding model (empty string if none configured).
    pub fn default_embedder_name(&self) -> &str {
        self.default_embedder_name.as_ref()
    }

    /// Resolve a model name (or `None` for the default) to an `EmbeddingModel`.
    ///
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
    /// scoped vector deletion on note delete — codex High 2 (PR #407)).
    /// The default model is included.
    pub fn registered_embedding_model_names(&self) -> Vec<String> {
        self.embedder_registry
            .read()
            .map(|reg| reg.names())
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
        // Try to resolve as a lattice alias first (normalises "paraphrase" →
        // "paraphrase-multilingual-minilm-l12-v2", etc.).  If that succeeds,
        // use the canonical key; otherwise fall back to the literal name so
        // custom providers registered with non-lattice names are reachable.
        let canonical_key = match parse_embedding_model_alias(name) {
            Some(model) => model.to_string(),
            None => name.to_owned(),
        };
        // Clone the entry before releasing the lock so we don't hold a
        // RwLockGuard across the async OnceCell initialisation (Send bound).
        let entry = {
            let registry = self.embedder_registry.read().map_err(|_| {
                crate::RuntimeError::Internal("embedder registry lock poisoned".into())
            })?;
            registry
                .get_entry(&canonical_key)
                .ok_or_else(|| crate::RuntimeError::UnknownModel(name.to_string()))?
        };
        entry.resolve().await
    }

    /// Register a custom embedding provider with this runtime.
    ///
    /// The provider is added to the shared [`EmbedderRegistry`] so all clones
    /// of this runtime see the new provider immediately. If a provider with the
    /// same name already exists it is replaced (last-writer wins — see
    /// [`EmbedderRegistry::register`] for the rationale).
    ///
    /// Packs should call this from [`PackRuntime::register_embedders`] (the
    /// hook is invoked by the transport during pack initialisation, before the
    /// first verb dispatch).
    ///
    /// [`EmbedderRegistry`]: crate::embedder_registry::EmbedderRegistry
    pub fn register_embedder(
        &self,
        provider: impl crate::embedder_registry::EmbedderProvider + 'static,
    ) {
        if let Ok(mut registry) = self.embedder_registry.write() {
            registry.register(provider);
        } else {
            tracing::warn!(
                "embedder registry lock poisoned — embedder {} not registered",
                std::any::type_name::<dyn crate::embedder_registry::EmbedderProvider>()
            );
        }
    }

    /// List registered embedding models via `SqlAccess`, routing through the
    /// existing connection pool rather than opening a fresh `Connection` per call.
    ///
    /// Optionally filter by `engine_name`. Returns an empty vec when the
    /// `_embedding_models` table does not yet exist (e.g. no migrations have run
    /// or no models have been registered). All other SQL errors are propagated.
    pub async fn list_embedding_models(
        &self,
        engine_filter: Option<&str>,
    ) -> RuntimeResult<Vec<khive_db::EmbeddingModelRegistryRecord>> {
        use khive_storage::{SqlStatement, SqlValue};

        let (sql_text, params) = if let Some(engine) = engine_filter {
            (
                "SELECT engine_name, model_id, key_version, dim, status, \
                 activated_at, superseded_at \
                 FROM _embedding_models WHERE engine_name = ?1 \
                 ORDER BY engine_name, activated_at IS NULL, activated_at"
                    .to_string(),
                vec![SqlValue::Text(engine.to_string())],
            )
        } else {
            (
                "SELECT engine_name, model_id, key_version, dim, status, \
                 activated_at, superseded_at \
                 FROM _embedding_models \
                 ORDER BY engine_name, activated_at IS NULL, activated_at"
                    .to_string(),
                vec![],
            )
        };

        let stmt = SqlStatement {
            sql: sql_text,
            params,
            label: Some("list_embedding_models".into()),
        };

        let mut reader = self
            .sql()
            .reader()
            .await
            .map_err(crate::RuntimeError::Storage)?;

        let rows = match reader.query_all(stmt).await {
            Ok(rows) => rows,
            Err(e) if e.to_string().contains("no such table: _embedding_models") => {
                return Ok(Vec::new())
            }
            Err(e) => return Err(crate::RuntimeError::Storage(e)),
        };

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            macro_rules! required_text {
                ($col:expr) => {
                    match row.get($col) {
                        Some(SqlValue::Text(s)) => s.clone(),
                        other => {
                            tracing::warn!(column = $col, value = ?other, "skipping registry row: unexpected type");
                            continue;
                        }
                    }
                };
            }
            let engine_name = required_text!("engine_name");
            let model_id = required_text!("model_id");
            let key_version = required_text!("key_version");
            let dimensions = match row.get("dim") {
                Some(SqlValue::Integer(n)) => match u32::try_from(*n) {
                    Ok(d) => d,
                    Err(_) => {
                        tracing::warn!(dim = n, "skipping registry row: dim out of u32 range");
                        continue;
                    }
                },
                other => {
                    tracing::warn!(column = "dim", value = ?other, "skipping registry row: unexpected type");
                    continue;
                }
            };
            let status = required_text!("status");
            let activated_at = match row.get("activated_at") {
                Some(SqlValue::Integer(n)) => Some(*n),
                _ => None,
            };
            let superseded_at = match row.get("superseded_at") {
                Some(SqlValue::Integer(n)) => Some(*n),
                _ => None,
            };
            records.push(khive_db::EmbeddingModelRegistryRecord {
                engine_name,
                model_id,
                key_version,
                dimensions,
                status,
                activated_at,
                superseded_at,
            });
        }

        Ok(records)
    }
}

/// Sanitize an embedding model into a valid SQL table suffix.
/// e.g. `bge-small-en-v1.5` -> `bge_small_en_v1_5`
pub(crate) fn vec_model_key(model: EmbeddingModel) -> String {
    sanitize_key(&model.to_string())
}

pub(crate) fn sanitize_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn build_embedder_registry(
    config: &RuntimeConfig,
) -> (crate::embedder_registry::EmbedderRegistry, Arc<str>) {
    use crate::embedder_registry::{EmbedderRegistry, LatticeEmbedderProvider};
    let mut registry = EmbedderRegistry::new();
    for model in configured_embedding_models(config) {
        registry.register(LatticeEmbedderProvider::new(model));
    }
    let default_embedder_name = config
        .embedding_model
        .map(|model| Arc::<str>::from(model.to_string()))
        .unwrap_or_else(|| Arc::<str>::from(""));
    (registry, default_embedder_name)
}

fn configured_embedding_models(config: &RuntimeConfig) -> Vec<EmbeddingModel> {
    let mut models = Vec::new();
    if let Some(model) = config.embedding_model {
        models.push(model);
    }
    models.extend(config.additional_embedding_models.iter().copied());
    models.sort_by_key(|model| model.to_string());
    models.dedup();
    models
}

fn register_configured_embedding_models(
    backend: &StorageBackend,
    config: &RuntimeConfig,
) -> RuntimeResult<()> {
    for model in configured_embedding_models(config) {
        backend.register_embedding_model(
            &model.to_string(),
            model.model_id(),
            model.key_version(),
            model.dimensions() as u32,
        )?;
    }
    Ok(())
}

/// Build a `RuntimeConfig` from a parsed `KhiveConfig`.
///
/// For each `[[engines]]` entry:
/// - The engine flagged `default = true` becomes `RuntimeConfig::embedding_model`.
/// - All other engines become `RuntimeConfig::additional_embedding_models`.
///
/// Model name validity is checked here: any engine whose `model` field cannot
/// be parsed via `parse_embedding_model_alias` is skipped with a warning.
///
/// If `khive_cfg.engines` is empty, the returned `RuntimeConfig` uses the
/// env-var-derived defaults from `RuntimeConfig::default()`.
///
/// When both a config file and `KHIVE_EMBEDDING_MODEL` env var are present,
/// the caller is responsible for emitting a warning that env vars are ignored.
/// This function purely converts `KhiveConfig` to `RuntimeConfig` fields.
pub fn runtime_config_from_khive_config(
    khive_cfg: &crate::engine_config::KhiveConfig,
    base: RuntimeConfig,
) -> RuntimeConfig {
    // Apply actor.id as default_namespace when present and valid.
    // KhiveConfig::validate() guarantees that actor.id, when present, is a
    // structurally valid Namespace — so the Err arm here is unreachable for
    // any config that passed load(). A panic here signals a caller contract
    // violation (passing an unvalidated config).
    let default_namespace = match khive_cfg.actor.id.as_deref() {
        Some(id) if !id.is_empty() => match Namespace::parse(id) {
            Ok(ns) => {
                tracing::debug!(actor_id = id, "actor.id from config sets default_namespace");
                ns
            }
            Err(e) => {
                panic!(
                    "actor.id {id:?} passed validation but Namespace::parse failed: {e}; \
                     this is a bug — KhiveConfig must be validated before calling \
                     runtime_config_from_khive_config"
                );
            }
        },
        _ => base.default_namespace.clone(),
    };

    if khive_cfg.engines.is_empty() {
        return RuntimeConfig {
            default_namespace,
            ..base
        };
    }

    let mut embedding_model: Option<EmbeddingModel> = None;
    let mut additional: Vec<EmbeddingModel> = Vec::new();

    for engine in &khive_cfg.engines {
        match parse_embedding_model_alias(&engine.model) {
            Some(model) => {
                if engine.default {
                    embedding_model = Some(model);
                } else {
                    additional.push(model);
                }
            }
            None => {
                tracing::warn!(
                    engine = %engine.name,
                    model = %engine.model,
                    "engine config: unknown model name; engine will be skipped"
                );
            }
        }
    }

    RuntimeConfig {
        embedding_model,
        additional_embedding_models: additional,
        default_namespace,
        ..base
    }
}

/// Parse a comma- or whitespace-separated list of embedding model names.
fn parse_embedding_model_list(s: &str) -> Vec<EmbeddingModel> {
    parse_pack_list(s)
        .into_iter()
        .filter_map(|raw| {
            let parsed = parse_embedding_model_alias(&raw);
            if parsed.is_none() && !raw.trim().is_empty() {
                // Codex Medium (PR #407): silent filter_map masks operator typos. Warn loudly
                // so misconfiguration surfaces at startup rather than as an UnknownModel error
                // at request time. We do not fail startup — a partially valid list still
                // produces a functional runtime — but the warning is unambiguous.
                tracing::warn!(
                    model = %raw,
                    "KHIVE_ADDITIONAL_EMBEDDING_MODELS contains unknown model name; ignored. \
                     Valid forms: short alias like 'paraphrase' or a fully-qualified key \
                     from lattice_embed::EmbeddingModel::from_str."
                );
            }
            parsed
        })
        .collect()
}

pub(crate) fn parse_embedding_model_alias(name: &str) -> Option<EmbeddingModel> {
    let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "paraphrase" => Some(EmbeddingModel::ParaphraseMultilingualMiniLmL12V2),
        _ => normalized.parse().ok(),
    }
}

// INLINE TEST JUSTIFICATION: tests here cover KhiveRuntime construction helpers
// (in-memory backend wiring, NamespaceToken::for_namespace) that are
// pub(crate)-only and cannot be called from the integration test crate.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_runtime_creates_successfully() {
        let rt = KhiveRuntime::memory().expect("memory runtime should create");
        assert!(rt.config().db_path.is_none());
    }

    #[test]
    fn file_runtime_creates_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let config = RuntimeConfig {
            db_path: Some(path.clone()),
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        };
        let rt = KhiveRuntime::new(config).expect("file runtime should create");
        assert!(path.exists());
        assert_eq!(rt.config().default_namespace.as_str(), "test");
    }

    #[test]
    fn from_backend_uses_provided_backend() {
        let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::new("lore"),
        };
        let rt = KhiveRuntime::from_backend(backend, config);
        assert_eq!(rt.backend_id().as_str(), "lore");
        assert!(rt.config().db_path.is_none());
    }

    #[test]
    fn backend_id_defaults_to_main() {
        let rt = KhiveRuntime::memory().unwrap();
        assert_eq!(rt.backend_id().as_str(), BackendId::MAIN);
    }

    #[test]
    fn store_accessors_return_ok() {
        let rt = KhiveRuntime::memory().unwrap();
        let tok = NamespaceToken::local();
        assert!(rt.entities(&tok).is_ok());
        assert!(rt.graph(&tok).is_ok());
        assert!(rt.notes(&tok).is_ok());
        assert!(rt.events(&tok).is_ok());
    }

    #[test]
    fn vectors_returns_unconfigured_without_model() {
        let rt = KhiveRuntime::memory().unwrap();
        let tok = NamespaceToken::local();
        match rt.vectors(&tok) {
            Err(crate::RuntimeError::Unconfigured(s)) => assert_eq!(s, "embedding_model"),
            Err(other) => panic!("expected Unconfigured, got {:?}", other),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn vec_model_key_sanitizes_dots_and_dashes() {
        assert_eq!(
            vec_model_key(EmbeddingModel::BgeSmallEnV15),
            "bge_small_en_v1_5"
        );
        assert_eq!(
            vec_model_key(EmbeddingModel::BgeBaseEnV15),
            "bge_base_en_v1_5"
        );
        assert_eq!(
            vec_model_key(EmbeddingModel::AllMiniLmL6V2),
            "all_minilm_l6_v2"
        );
    }

    #[test]
    fn default_config_uses_allow_all_gate() {
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.default_namespace.as_str(), "local");
        let _: GateRef = cfg.gate.clone();
    }

    #[test]
    fn parse_pack_list_handles_comma_and_whitespace() {
        assert_eq!(parse_pack_list("kg"), vec!["kg".to_string()]);
        assert_eq!(
            parse_pack_list("kg,gtd"),
            vec!["kg".to_string(), "gtd".to_string()]
        );
        assert_eq!(
            parse_pack_list("  kg ,  gtd  "),
            vec!["kg".to_string(), "gtd".to_string()]
        );
        assert_eq!(
            parse_pack_list("kg gtd"),
            vec!["kg".to_string(), "gtd".to_string()]
        );
        assert_eq!(parse_pack_list(",,"), Vec::<String>::new());
        assert_eq!(parse_pack_list(""), Vec::<String>::new());
    }

    #[test]
    fn default_config_packs_loads_all_production_packs() {
        let prior = std::env::var("KHIVE_PACKS").ok();
        // SAFETY: test function runs single-threaded; no other threads read or write KHIVE_PACKS.
        unsafe {
            std::env::remove_var("KHIVE_PACKS");
        }
        let cfg = RuntimeConfig::default();
        assert!(cfg.packs.contains(&"kg".to_string()));
        assert!(cfg.packs.contains(&"gtd".to_string()));
        assert!(cfg.packs.contains(&"memory".to_string()));
        assert!(cfg.packs.contains(&"brain".to_string()));
        assert!(cfg.packs.contains(&"comm".to_string()));
        assert!(cfg.packs.contains(&"schedule".to_string()));
        assert!(cfg.packs.contains(&"knowledge".to_string()));
        assert_eq!(cfg.packs.len(), 7);
        if let Some(v) = prior {
            // SAFETY: single-threaded test cleanup; restores KHIVE_PACKS to its prior value.
            unsafe {
                std::env::set_var("KHIVE_PACKS", v);
            }
        }
    }

    #[test]
    fn default_config_uses_minilm_when_env_unset() {
        let prior = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
        // SAFETY: tests are serial by default for env mutation here; if other tests
        // mutate this var, mark them with the same scope.
        unsafe {
            std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.embedding_model, Some(EmbeddingModel::AllMiniLmL6V2));
        if let Some(v) = prior {
            // SAFETY: single-threaded test cleanup; restores KHIVE_EMBEDDING_MODEL to its prior value.
            unsafe {
                std::env::set_var("KHIVE_EMBEDDING_MODEL", v);
            }
        }
    }

    // ---- Actor config tests ----

    use crate::engine_config::{ActorConfig, KhiveConfig};

    fn khive_cfg_with_actor(id: &str) -> KhiveConfig {
        KhiveConfig {
            engines: vec![],
            actor: ActorConfig {
                id: Some(id.to_string()),
                display_name: None,
            },
        }
    }

    #[test]
    fn runtime_config_from_khive_config_applies_actor_id_as_default_namespace() {
        let base = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        };
        let cfg = khive_cfg_with_actor("lambda:khive");
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(result.default_namespace.as_str(), "lambda:khive");
    }

    #[test]
    fn runtime_config_from_khive_config_empty_actor_id_keeps_base_namespace() {
        let base = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("lambda:base").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        };
        let cfg = KhiveConfig {
            engines: vec![],
            actor: ActorConfig {
                id: Some(String::new()),
                display_name: None,
            },
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.default_namespace.as_str(),
            "lambda:base",
            "empty actor.id must not override base namespace"
        );
    }

    #[test]
    fn runtime_config_from_khive_config_absent_actor_id_keeps_base_namespace() {
        let base = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::parse("lambda:base").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        };
        let cfg = KhiveConfig::default(); // no actor.id
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.default_namespace.as_str(),
            "lambda:base",
            "absent actor.id must not override base namespace"
        );
    }

    #[test]
    fn runtime_config_from_khive_config_actor_id_with_engines() {
        let base = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        };
        let cfg = KhiveConfig {
            engines: vec![crate::engine_config::EngineConfig {
                name: "default".to_string(),
                model: "all-minilm-l6-v2".to_string(),
                default: true,
                fusion_weight: None,
                dims: None,
            }],
            actor: ActorConfig {
                id: Some("lambda:test".to_string()),
                display_name: None,
            },
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(result.default_namespace.as_str(), "lambda:test");
        assert!(result.embedding_model.is_some());
    }

    // ---- list_embedding_models tests ----

    #[tokio::test]
    async fn list_embedding_models_returns_empty_when_table_absent() {
        // A brand-new in-memory runtime has migrations applied, so _embedding_models
        // IS created. But with no rows inserted, the result must be empty.
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let records = rt
            .list_embedding_models(None)
            .await
            .expect("list ok on empty table");
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn list_embedding_models_returns_row_after_insert() {
        use khive_storage::{SqlStatement, SqlValue};

        let rt = KhiveRuntime::memory().expect("memory runtime");
        let sql = rt.sql();

        let now = 1_000_000i64;
        let id = uuid::Uuid::new_v4();
        let canonical_key = b"test_engine:test-model-v1:v1:384".to_vec();

        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO _embedding_models \
                      (id, engine_name, model_id, key_version, dim, output_dim, status, \
                       activated_at, superseded_at, superseded_by, canonical_key, created_at) \
                      VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, NULL, NULL, ?8, ?9)"
                    .into(),
                params: vec![
                    SqlValue::Blob(id.as_bytes().to_vec()),
                    SqlValue::Text("test_engine".into()),
                    SqlValue::Text("test-model-v1".into()),
                    SqlValue::Text("v1".into()),
                    SqlValue::Integer(384),
                    SqlValue::Text("active".into()),
                    SqlValue::Integer(now),
                    SqlValue::Blob(canonical_key),
                    SqlValue::Integer(now),
                ],
                label: None,
            })
            .await
            .expect("insert row");
        drop(writer);

        let records = rt.list_embedding_models(None).await.expect("list ok");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].engine_name, "test_engine");
        assert_eq!(records[0].model_id, "test-model-v1");
        assert_eq!(records[0].key_version, "v1");
        assert_eq!(records[0].dimensions, 384);
        assert_eq!(records[0].status, "active");

        // engine filter — match
        let filtered = rt
            .list_embedding_models(Some("test_engine"))
            .await
            .expect("filter ok");
        assert_eq!(filtered.len(), 1);

        // engine filter — no match
        let no_match = rt
            .list_embedding_models(Some("other_engine"))
            .await
            .expect("no-match ok");
        assert!(no_match.is_empty());
    }
}
