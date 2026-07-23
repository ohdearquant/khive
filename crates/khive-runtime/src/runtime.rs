//! KhiveRuntime — composable handle to all storage capabilities.
//!
//! `RuntimeConfig`, `BackendId`, `NamespaceToken`, and embedding model helpers
//! live in `super::config` and are re-exported from here.

use std::sync::{Arc, RwLock};

use khive_db::StorageBackend;
#[cfg(test)]
use khive_gate::AllowAllGate;
use khive_gate::GateRequest;
use khive_storage::{EntityStore, Event, EventStore, GraphStore, NoteStore, SqlAccess};
use khive_types::{EdgeEndpointRule, EventKind, Namespace, SubstrateKind};
use lattice_embed::{EmbeddingModel, EmbeddingService};

use crate::config::{
    build_embedder_registry, parse_embedding_model_alias, register_configured_embedding_models,
    sanitize_key, vec_model_key,
};
use crate::error::{RuntimeError, RuntimeResult};

/// Callback type for pack-installed entity-type validators.
///
/// Receives `(kind, entity_type)` and returns the normalised type string,
/// or `RuntimeError::InvalidInput` if the type is not registered for that kind.
/// When `entity_type` is `None`, the implementation must return `Ok(None)`.
pub type EntityTypeValidatorFn =
    Arc<dyn Fn(&str, Option<&str>) -> Result<Option<String>, RuntimeError> + Send + Sync>;

/// Callback type for a pack-installed note-mutation hook.
///
/// Invoked by `update_note` (when the note's text/embedding actually
/// changed) and `delete_note` (soft or hard) with `(note_kind, note_id)`,
/// after the mutation has been durably applied. Returns a boxed future so
/// the hook can await async cache-invalidation work (e.g.
/// `khive-pack-memory`'s ANN warm-cache generation bump) without
/// `khive-runtime` depending on any pack crate: dependencies point the
/// other way, so the runtime exposes an extension point and the pack
/// installs into it, same shape as `EntityTypeValidatorFn`, just async.
pub type NoteMutationHookFn = Arc<
    dyn Fn(String, uuid::Uuid) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

pub use crate::config::{
    assert_captured_db_anchor_consistent, assert_db_anchor_consistent, expand_tilde,
    parse_pack_list, resolve_db_anchor, resolve_project_actor_id, runtime_config_from_khive_config,
    BackendId, NamespaceToken, RuntimeConfig,
};

// ---- KhiveRuntime ----

/// Composable runtime handle used by the MCP server.
///
/// Wraps a `StorageBackend` and provides namespace-scoped accessor methods
/// for each storage capability, plus a lazily-loaded embedder.
#[derive(Clone)]
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    /// When `Some`, holds the main backend so that `core()` can return a
    /// main-bound runtime handle without constructing a new connection.
    /// `None` when this runtime is already bound to the main backend.
    core_backend: Option<Arc<StorageBackend>>,
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
    /// Pack-installed entity-type validator.
    ///
    /// When `Some`, `create_many` calls this function to validate and normalise
    /// each `(kind, entity_type)` pair before writing. When `None` (bare runtime
    /// without packs), entity-type validation is skipped — the pack handler layer
    /// is the primary enforcement point, same as for `valid_entity_kinds`.
    entity_type_validator: Arc<RwLock<Option<EntityTypeValidatorFn>>>,
    /// Pack-installed note-mutation hook.
    ///
    /// When `Some`, `update_note` (on text change) and `delete_note` (soft
    /// or hard) call this after the mutation is durably applied, so a pack
    /// that caches derived state keyed by note content (e.g. `khive-pack-memory`'s
    /// warm ANN index) can invalidate/advance its own generation counter even
    /// when the mutation arrived through a different pack's verb (e.g. KG's
    /// `update`/`delete` on a `kind="memory"` note) that has no dependency on
    /// the reacting pack. `None` when no pack installs one (bare runtime, or
    /// no pack cares about note-mutation notifications) — the call becomes a
    /// no-op check of an `Option`.
    note_mutation_hook: Arc<RwLock<Option<NoteMutationHookFn>>>,
    /// The config-resolved `BlobStore` (ADR-111 Amendment 2), installed by
    /// the boot path (`khive-mcp`'s single- and multi-backend startup paths)
    /// via [`install_blob_store`](Self::install_blob_store) once `khive.toml`'s
    /// `[storage.blob]` selection has been resolved through
    /// `khive_runtime::resolve_blob_store`. `None` until installed — every
    /// constructor here defaults it unset rather than eagerly resolving a
    /// filesystem default, because many callers (unit tests, `memory()`) use
    /// an in-memory backend with no blob root configured at all.
    blob_store: Arc<RwLock<Option<Arc<dyn khive_storage::BlobStore>>>>,
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
        // Migrations must run before any pack handler touches the DB; idempotent;
        // failure aborts construction with a clear error.
        {
            let mut writer = backend.pool().try_writer()?;
            khive_db::run_migrations(writer.conn_mut())?;
        }
        register_configured_embedding_models(&backend, &config)?;
        let (registry, default_embedder_name) = build_embedder_registry(&config);
        Ok(Self {
            backend: Arc::new(backend),
            core_backend: None,
            config,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
            entity_type_validator: Arc::new(RwLock::new(None)),
            note_mutation_hook: Arc::new(RwLock::new(None)),
            blob_store: Arc::new(RwLock::new(None)),
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
            core_backend: None,
            config,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
            entity_type_validator: Arc::new(RwLock::new(None)),
            note_mutation_hook: Arc::new(RwLock::new(None)),
            blob_store: Arc::new(RwLock::new(None)),
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
            core_backend: None,
            config,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
            entity_type_validator: Arc::new(RwLock::new(None)),
            note_mutation_hook: Arc::new(RwLock::new(None)),
            blob_store: Arc::new(RwLock::new(None)),
        }
    }

    /// Wire this runtime as a secondary-backend runtime pointing at `core`.
    ///
    /// After this call, `self.core()` returns a handle to `core` rather than
    /// cloning `self`. The caller (the boot path, not pack code) is responsible
    /// for passing the correct main backend.
    ///
    /// Panics in debug builds if `self.config.backend_id == BackendId::MAIN`,
    /// because the main runtime does not need a core pointer.
    pub fn with_core_backend(mut self, core: Arc<StorageBackend>) -> Self {
        debug_assert_ne!(
            self.config.backend_id.as_str(),
            BackendId::MAIN,
            "with_core_backend must not be called on the main runtime"
        );
        self.core_backend = Some(core);
        self
    }

    /// Return a runtime handle bound to the main (shared-graph) backend.
    ///
    /// When `self` is already the main runtime (`core_backend` is `None`),
    /// this returns a clone of `self` — no new backend reference is acquired.
    ///
    /// When `self` is a secondary-backend runtime (`core_backend` is `Some`),
    /// this returns a new `KhiveRuntime` backed by the main
    /// `Arc<StorageBackend>` and sharing all registry state (`embedder_registry`,
    /// `edge_rules`, `valid_entity_kinds`, `valid_note_kinds`,
    /// `entity_type_validator`, `note_mutation_hook`) with `self`.
    /// No database I/O occurs; no embedding models are reloaded.
    ///
    /// Use `core()` for notes and entities that must reside in the shared graph
    /// so that `memory.recall`, cross-pack search, and `annotates` edges work.
    /// Use `self` (or `self.sql()`) for pack-auxiliary bulk tables.
    ///
    /// Handlers that call `core()` more than once per request or loop should bind
    /// `let core = self.core();` once and reuse it, since each call clones
    /// `RuntimeConfig` (a heap-allocated struct containing `Vec<String>` fields).
    pub fn core(&self) -> KhiveRuntime {
        match &self.core_backend {
            None => self.clone(),
            Some(main_arc) => {
                let mut core_config = self.config.clone();
                core_config.backend_id = BackendId::main();
                KhiveRuntime {
                    backend: main_arc.clone(),
                    core_backend: None,
                    config: core_config,
                    embedder_registry: self.embedder_registry.clone(),
                    default_embedder_name: self.default_embedder_name.clone(),
                    edge_rules: self.edge_rules.clone(),
                    valid_entity_kinds: self.valid_entity_kinds.clone(),
                    valid_note_kinds: self.valid_note_kinds.clone(),
                    entity_type_validator: self.entity_type_validator.clone(),
                    note_mutation_hook: self.note_mutation_hook.clone(),
                    blob_store: self.blob_store.clone(),
                }
            }
        }
    }

    /// Create an in-memory runtime (for tests and ephemeral use).
    pub fn memory() -> RuntimeResult<Self> {
        Self::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            brain_profile: None,
            actor_id: None,
            ..RuntimeConfig::no_embeddings()
        })
    }

    /// Return the [`BackendId`] for this runtime's backend.
    ///
    /// Used by `SubstrateCoordinator` in `kkernel`
    /// to identify which backend owns a given node, and to detect cross-backend merges.
    pub fn backend_id(&self) -> &BackendId {
        &self.config.backend_id
    }

    /// Return the extra-visible namespaces assembled at config load.
    ///
    /// OSS dispatch uses this set to widen the default multi-record read scope
    /// to `['local'] ∪ visible_namespaces`. Writes are unchanged: always
    /// pinned to `'local'`. This set is also available as gate/cloud policy
    /// input.
    pub fn visible_namespaces(&self) -> &[Namespace] {
        &self.config.visible_namespaces
    }

    /// Return a reference to the runtime config.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Return a reference to the underlying storage backend.
    pub fn backend(&self) -> &StorageBackend {
        &self.backend
    }

    /// Return the directory containing the backend's database file, or `None`
    /// for an in-memory backend.
    pub fn backend_data_dir(&self) -> Option<std::path::PathBuf> {
        self.backend.data_dir()
    }

    /// Root directory for this database's ANN segment tree (`<db-file>.ann/`
    /// beside the file), or `None` for an in-memory backend. Scoped to the
    /// database file itself so two databases sharing a parent directory can
    /// never adopt each other's segments.
    pub fn backend_ann_root(&self) -> Option<std::path::PathBuf> {
        self.backend.ann_root()
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

    /// Output dimensions for a named embedding model, resolved from the
    /// embedder registry alone — no storage access. Mirrors
    /// [`vectors_for_model`](Self::vectors_for_model)'s resolution order:
    /// lattice aliases route through the enum when registered, otherwise the
    /// custom provider's declared `dimensions()`. `None` when no such model
    /// is registered.
    pub fn embedder_dimensions(&self, model_name: &str) -> Option<usize> {
        if let Some(model) = parse_embedding_model_alias(model_name) {
            let key = model.to_string();
            let in_registry = self
                .embedder_registry
                .read()
                .map(|reg| reg.contains(&key))
                .unwrap_or(false);
            if in_registry {
                return Some(model.dimensions());
            }
        }
        self.embedder_registry
            .read()
            .ok()?
            .get_provider(model_name)
            .map(|p| p.dimensions())
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

    /// Get a TextSearch index for the entity corpus (single shared table).
    pub fn text(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let _ = token;
        Ok(self.backend.text("entities")?)
    }

    /// Get a TextSearch index for the notes corpus (single shared table).
    pub fn text_for_notes(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let _ = token;
        Ok(self.backend.text("notes")?)
    }

    /// Mint an authorization token for the given namespace.
    ///
    /// Consults the configured [`crate::Gate`] before minting. With the default
    /// `AllowAllGate` this always succeeds. When a real policy-backed gate is
    /// installed, this method enforces it and returns `PermissionDenied` on
    /// denial.
    ///
    /// The returned token's read visibility set defaults to `[ns]` — identical
    /// to the pre-visibility-set behaviour. Use [`Self::authorize_with_visibility`]
    /// to mint a token that can read additional namespaces.
    ///
    /// When `actor_id` is configured in `RuntimeConfig`, the token carries that
    /// actor label so that `comm.inbox` filters by `to_actor`. When
    /// unconfigured, the token carries `ActorRef::anonymous()` and inbox falls
    /// back to party-line behavior.
    pub fn authorize(&self, ns: Namespace) -> RuntimeResult<NamespaceToken> {
        let actor = crate::actor_identity::resolve_actor(self.config.actor_id.as_deref());
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

    /// Mint an authorization token with an explicit read-visibility set.
    ///
    /// `primary` is the **write namespace** — all records created via the
    /// returned token land there. `extra_visible` lists additional namespaces
    /// the token may read. The primary is always included in the visible set
    /// regardless of `extra_visible`.
    ///
    /// Usage (lambda:leo reading both leo and khive namespaces):
    /// ```rust,ignore
    /// let tok = rt.authorize_with_visibility(
    ///     Namespace::parse("lambda:leo").unwrap(),
    ///     vec![Namespace::parse("lambda:khive").unwrap()],
    /// )?;
    /// ```
    pub fn authorize_with_visibility(
        &self,
        primary: Namespace,
        extra_visible: Vec<Namespace>,
    ) -> RuntimeResult<NamespaceToken> {
        let actor = crate::actor_identity::resolve_actor(self.config.actor_id.as_deref());
        let req = GateRequest::new(
            actor.clone(),
            primary.clone(),
            "authorize",
            serde_json::Value::Null,
        );
        match self.config.gate.check(&req) {
            Ok(ref decision) if decision.is_allow() => {
                if let khive_gate::GateDecision::Allow { ref obligations } = decision {
                    if !obligations.is_empty() {
                        tracing::debug!(
                            namespace = %primary.as_str(),
                            "authorize_with_visibility: obligations={:?}",
                            obligations
                        );
                    }
                }
                Ok(NamespaceToken::mint_with_visibility(
                    primary,
                    extra_visible,
                    actor,
                ))
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

    /// Install the config-resolved `BlobStore` (ADR-111 Amendment 2).
    ///
    /// Called by the boot path (`khive-mcp`'s single- and multi-backend
    /// startup paths, `crates/khive-mcp/src/serve.rs`) once
    /// `khive_runtime::resolve_blob_store` has selected the backend
    /// `khive.toml`'s `[storage.blob]` section requests. Idempotent: a later
    /// call overwrites the previous handle.
    pub fn install_blob_store(&self, store: Arc<dyn khive_storage::BlobStore>) {
        if let Ok(mut guard) = self.blob_store.write() {
            *guard = Some(store);
        }
    }

    /// Return the installed `BlobStore`, if the boot path resolved and
    /// installed one. `None` when no `[storage.blob]` selection was ever
    /// installed — e.g. a bare/test runtime constructed without going
    /// through the `khive-mcp` boot path.
    pub fn blob_store(&self) -> Option<Arc<dyn khive_storage::BlobStore>> {
        self.blob_store.read().ok().and_then(|guard| guard.clone())
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

    /// Install a pack-supplied entity-type validator.
    ///
    /// Called by the `KgPack` during registration so that `create_many` can validate
    /// `entity_type` values at the runtime layer, closing the hole where direct Rust
    /// callers bypass the handler-layer `validate_entity_type` check.
    ///
    /// The callback receives `(kind, entity_type)` and returns the normalised type
    /// string, or `RuntimeError::InvalidInput` if the type is not registered for that
    /// kind. Passing `entity_type = None` must return `Ok(None)`.
    pub fn install_entity_type_validator(&self, f: EntityTypeValidatorFn) {
        if let Ok(mut guard) = self.entity_type_validator.write() {
            *guard = Some(f);
        }
    }

    /// Validate and normalise `entity_type` through the pack-installed validator.
    ///
    /// Returns `Ok(entity_type)` when no validator is installed (bare runtime).
    /// Returns `InvalidInput` when a validator is installed and rejects the type.
    pub(crate) fn validate_entity_type_for_kind(
        &self,
        kind: &str,
        entity_type: Option<&str>,
    ) -> crate::RuntimeResult<Option<String>> {
        let guard = self.entity_type_validator.read().map_err(|_| {
            crate::RuntimeError::Internal("entity type validator lock poisoned".into())
        })?;
        match guard.as_ref() {
            None => Ok(entity_type.map(str::to_string)),
            Some(validate) => validate(kind, entity_type),
        }
    }

    /// Install a pack-owned note-mutation hook.
    ///
    /// Overwrites any previously-installed hook, same single-slot semantics
    /// as [`install_entity_type_validator`](Self::install_entity_type_validator).
    /// In practice only one pack (`khive-pack-memory`) installs one today;
    /// if a second pack ever needs this, the slot should be widened to a
    /// `Vec` at that point rather than silently overwritten.
    pub fn install_note_mutation_hook(&self, f: NoteMutationHookFn) {
        if let Ok(mut guard) = self.note_mutation_hook.write() {
            *guard = Some(f);
        }
    }

    /// Invoke the pack-installed note-mutation hook, if any.
    ///
    /// `kind` is the note's `kind` string (e.g. `"memory"`); `id` is the
    /// note's UUID. No-op when no hook is installed (bare runtime, or no
    /// pack cares). Errors inside the hook are the hook's own concern to
    /// handle/log — this call site cannot propagate a failure without
    /// changing `update_note`/`delete_note`'s already-committed success
    /// return value.
    pub(crate) async fn fire_note_mutation_hook(&self, kind: &str, id: uuid::Uuid) {
        let hook = self
            .note_mutation_hook
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        if let Some(hook) = hook {
            hook(kind.to_string(), id).await;
        }
    }

    /// Snapshot of currently-installed pack edge rules.
    ///
    /// This is the same composed rule set `validate_edge_relation_endpoints`
    /// consults via `pack_rule_allows` when accepting/rejecting an edge. Public
    /// so pack-layer error-hint code (e.g. `khive-pack-kg`'s
    /// `valid_relations_for_entity_pair`) can derive hints from the exact
    /// source the validator uses, rather than maintaining a separate
    /// hand-authored table that can drift out of sync.
    pub fn pack_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.edge_rules
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Borrow the installed pack edge rules for a synchronous calculation.
    pub(crate) fn with_pack_edge_rules<T>(&self, f: impl FnOnce(&[EdgeEndpointRule]) -> T) -> T {
        match self.edge_rules.read() {
            Ok(rules) => f(&rules),
            Err(_) => f(&[]),
        }
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
    /// scoped vector deletion on note delete). The default model is included.
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
        self.embedder_inner(name, None).await
    }

    pub(crate) async fn embedder_with_token(
        &self,
        token: &NamespaceToken,
        name: &str,
    ) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.embedder_inner(name, Some(token)).await
    }

    async fn embedder_inner(
        &self,
        name: &str,
        token: Option<&NamespaceToken>,
    ) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        // Fall back to the literal name (not the alias table) so custom
        // providers registered with non-lattice names stay reachable.
        let canonical_key = match parse_embedding_model_alias(name) {
            Some(model) => model.to_string(),
            None => name.to_owned(),
        };
        // Clone the entry so we don't hold the RwLockGuard across the
        // async OnceCell initialisation (Send bound).
        let entry = {
            let registry = self.embedder_registry.read().map_err(|_| {
                crate::RuntimeError::Internal("embedder registry lock poisoned".into())
            })?;
            registry
                .get_entry(&canonical_key)
                .ok_or_else(|| crate::RuntimeError::UnknownModel(name.to_string()))?
        };
        let (service, init_duration_us) = entry.resolve().await?;
        if let Some(duration_us) = init_duration_us {
            if let Some(token) = token {
                self.emit_embedder_initialized(token, &canonical_key, duration_us)
                    .await;
            } else if let Ok(token) = self.authorize(self.config.default_namespace.clone()) {
                self.emit_embedder_initialized(&token, &canonical_key, duration_us)
                    .await;
            }
        }
        Ok(service)
    }

    async fn emit_embedder_initialized(
        &self,
        token: &NamespaceToken,
        model_name: &str,
        duration_us: i64,
    ) {
        let Ok(store) = self.events(token) else {
            return;
        };
        let event = Event::new(
            token.namespace().as_str(),
            "embedder.init",
            EventKind::EmbedderInitialized,
            SubstrateKind::Event,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_payload(serde_json::json!({
            "model_name": model_name,
            "duration_us": duration_us,
        }))
        .with_duration_us(duration_us);
        if let Err(err) = store.append_event(event).await {
            tracing::warn!(error = %err, model_name, "embedder initialization event append failed");
        }
    }

    /// Register a custom embedding provider with this runtime.
    ///
    /// The provider is added to the shared [`EmbedderRegistry`] so all clones
    /// of this runtime see the new provider immediately. If a provider with the
    /// same name already exists it is replaced (last-writer wins — see
    /// [`crate::EmbedderRegistry::register`] for the rationale).
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

// INLINE TEST JUSTIFICATION: tests here cover KhiveRuntime construction helpers
// (in-memory backend wiring, NamespaceToken::for_namespace) that are
// pub(crate)-only and cannot be called from the integration test crate.
#[cfg(test)]
mod tests {
    use super::*;
    use khive_gate::GateRef;
    use serial_test::serial;

    #[test]
    fn memory_runtime_creates_successfully() {
        let rt = KhiveRuntime::memory().expect("memory runtime should create");
        assert!(rt.config().db_path.is_none());
    }

    #[test]
    fn backend_data_dir_returns_none_for_memory_backend() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        assert!(rt.backend_data_dir().is_none());
    }

    #[test]
    fn backend_data_dir_returns_parent_dir_for_file_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let config = RuntimeConfig {
            git_write: Default::default(),
            db_path: Some(path),
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        };
        let rt = KhiveRuntime::new(config).expect("file runtime");
        let data_dir = rt
            .backend_data_dir()
            .expect("file backend must return Some");
        assert_eq!(data_dir, dir.path());
    }

    #[test]
    fn backend_data_dir_returns_none_for_from_backend_with_memory() {
        let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
        let config = RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        };
        let rt = KhiveRuntime::from_backend(backend, config);
        assert!(rt.backend_data_dir().is_none());
    }

    #[test]
    fn file_runtime_creates_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let config = RuntimeConfig {
            git_write: Default::default(),
            db_path: Some(path.clone()),
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        };
        let rt = KhiveRuntime::new(config).expect("file runtime should create");
        assert!(path.exists());
        assert_eq!(rt.config().default_namespace.as_str(), "test");
    }

    /// A `~/`-prefixed `--db`/`KHIVE_DB` override must resolve, boot, and
    /// fingerprint identically to the equivalent absolute path. Before this
    /// fix, `resolve_db_anchor` left a leading `~` unexpanded in
    /// `RuntimeConfig.db_path`, so single-backend boot (`KhiveRuntime::new`)
    /// opened a literal `./~/...` file under the process cwd while
    /// `compute_config_id` (which canonicalizes/expands separately) still
    /// fingerprinted the real `$HOME` path — two processes pointed at the
    /// same logical database could open different files yet share a
    /// `config_id`, letting daemon dispatch route requests to the wrong one.
    #[test]
    #[serial]
    fn tilde_prefixed_db_override_resolves_and_boots_like_the_absolute_equivalent() {
        let original_home = std::env::var_os("HOME");
        let original_cwd = std::env::current_dir().expect("read cwd");
        let home_dir = tempfile::tempdir().expect("home tempdir");
        let work_dir = tempfile::tempdir().expect("work tempdir");
        std::env::set_var("HOME", home_dir.path());
        std::env::set_current_dir(work_dir.path()).expect("chdir into isolated work dir");

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let tilde_anchor = crate::config::resolve_db_anchor(Some("~/data.db"))
                .expect("an explicit path always anchors");
            let expected = home_dir.path().join("data.db");
            assert_eq!(
                tilde_anchor, expected,
                "resolve_db_anchor must expand a leading ~ to $HOME before it ever \
                 reaches RuntimeConfig.db_path"
            );

            let absolute_anchor = crate::config::resolve_db_anchor(Some(
                expected.to_str().expect("utf8 tempdir path"),
            ))
            .expect("an explicit path always anchors");
            assert_eq!(
                tilde_anchor, absolute_anchor,
                "a ~-prefixed override and its equivalent absolute path must resolve to \
                 the identical anchor"
            );

            let make_config = |db_path: std::path::PathBuf| RuntimeConfig {
                git_write: Default::default(),
                db_path: Some(db_path),
                default_namespace: Namespace::local(),
                embedding_model: None,
                additional_embedding_models: vec![],
                gate: Arc::new(AllowAllGate),
                packs: vec!["kg".to_string()],
                backend_id: BackendId::main(),
                brain_profile: None,
                visible_namespaces: vec![],
                allowed_outbound_namespaces: vec![],
                actor_id: None,
            };

            let tilde_cfg = make_config(tilde_anchor.clone());

            let rt = KhiveRuntime::new(tilde_cfg).expect("boot must open the expanded path");
            assert_eq!(
                rt.backend_data_dir().expect("file backend"),
                home_dir.path(),
                "single-backend boot must open the file under the expanded $HOME \
                 directory, not a literal ~ path relative to cwd"
            );
            assert!(
                expected.exists(),
                "the database file must be created at the expanded $HOME path"
            );
            assert!(
                !work_dir.path().join("~").exists(),
                "boot must never create a literal '~' directory under the process cwd"
            );
        }));

        match &original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::env::set_current_dir(&original_cwd);
        outcome.expect("test body panicked");
    }

    #[test]
    fn from_backend_uses_provided_backend() {
        let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
        let config = RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::new("lore"),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
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
    fn default_config_packs_loads_kg_only() {
        let prior = std::env::var("KHIVE_PACKS").ok();
        // SAFETY: test function runs single-threaded; no other threads read or write KHIVE_PACKS.
        unsafe {
            std::env::remove_var("KHIVE_PACKS");
        }
        // The open-source distribution ships a single production pack: kg.
        // Extension packs are commercially licensed, distributed separately,
        // and opt in via KHIVE_PACKS / --pack against a binary that links them.
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.packs, vec!["kg".to_string()]);
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
                ..Default::default()
            },
            ..KhiveConfig::default()
        }
    }

    #[test]
    fn runtime_config_from_khive_config_actor_id_does_not_override_default_namespace() {
        // `[actor] id` must not set `default_namespace`: writes stay pinned to
        // `local`. A non-`'local'` actor.id is folded into the default read
        // visible-set, but that does not change default_namespace. This test
        // asserts the write-routing invariant only.
        let base = RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        };
        let cfg = khive_cfg_with_actor("lambda:khive");
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.default_namespace.as_str(),
            "local",
            "actor.id must not become default_namespace (ADR-007 Rev 4 Rule 0); writes pin to local"
        );
    }

    #[test]
    fn runtime_config_from_khive_config_empty_actor_id_keeps_base_namespace() {
        let base = RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::parse("lambda:base").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        };
        let cfg = KhiveConfig {
            engines: vec![],
            actor: ActorConfig {
                id: Some(String::new()),
                display_name: None,
                ..Default::default()
            },
            ..KhiveConfig::default()
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
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::parse("lambda:base").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
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
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
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
                ..Default::default()
            },
            ..KhiveConfig::default()
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.default_namespace.as_str(),
            "local",
            "actor.id must not override default_namespace (ADR-007 Rev 4 Rule 0); \
             writes pin to local; engine config is still applied"
        );
        assert!(result.embedding_model.is_some());
    }

    // ---- base.actor_id (env-resolved actor) preservation tests ----
    //
    // Regression coverage: a project config found without an `[actor] id` used
    // to silently drop `base.actor_id` (e.g. the value `RuntimeConfig::default()`
    // read from `KHIVE_ACTOR`) because both return arms spread an unconditional
    // `actor_id: None` over `..base`. The fix falls back to `base.actor_id`
    // when the TOML supplies no `[actor] id`, in both arms.

    #[test]
    #[serial]
    fn runtime_config_from_khive_config_engines_present_preserves_env_actor_when_toml_has_none() {
        let prior = std::env::var("KHIVE_ACTOR").ok();
        // SAFETY: test is #[serial]; no other test in this crate reads/writes KHIVE_ACTOR.
        unsafe {
            std::env::set_var("KHIVE_ACTOR", "lambda:test-env-actor");
        }
        let base = RuntimeConfig::default();
        assert_eq!(base.actor_id.as_deref(), Some("lambda:test-env-actor"));

        let cfg = KhiveConfig {
            engines: vec![crate::engine_config::EngineConfig {
                name: "default".to_string(),
                model: "all-minilm-l6-v2".to_string(),
                default: true,
                fusion_weight: None,
                dims: None,
            }],
            actor: ActorConfig::default(), // no [actor] id
            ..KhiveConfig::default()
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.actor_id.as_deref(),
            Some("lambda:test-env-actor"),
            "engines-present arm must preserve base.actor_id (env actor) when TOML has no [actor] id"
        );

        // SAFETY: restores prior KHIVE_ACTOR value (test cleanup).
        unsafe {
            match prior {
                Some(v) => std::env::set_var("KHIVE_ACTOR", v),
                None => std::env::remove_var("KHIVE_ACTOR"),
            }
        }
    }

    #[test]
    #[serial]
    fn runtime_config_from_khive_config_engines_empty_preserves_env_actor_when_toml_has_none() {
        let prior = std::env::var("KHIVE_ACTOR").ok();
        // SAFETY: test is #[serial]; no other test in this crate reads/writes KHIVE_ACTOR.
        unsafe {
            std::env::set_var("KHIVE_ACTOR", "lambda:test-env-actor");
        }
        let base = RuntimeConfig::default();
        assert_eq!(base.actor_id.as_deref(), Some("lambda:test-env-actor"));

        let cfg = KhiveConfig {
            engines: vec![],
            actor: ActorConfig::default(), // no [actor] id
            ..KhiveConfig::default()
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.actor_id.as_deref(),
            Some("lambda:test-env-actor"),
            "engines-empty early-return arm must preserve base.actor_id (env actor) when TOML has no [actor] id"
        );

        // SAFETY: restores prior KHIVE_ACTOR value (test cleanup).
        unsafe {
            match prior {
                Some(v) => std::env::set_var("KHIVE_ACTOR", v),
                None => std::env::remove_var("KHIVE_ACTOR"),
            }
        }
    }

    #[test]
    #[serial]
    fn runtime_config_from_khive_config_toml_actor_wins_over_env_actor() {
        let prior = std::env::var("KHIVE_ACTOR").ok();
        // SAFETY: test is #[serial]; no other test in this crate reads/writes KHIVE_ACTOR.
        unsafe {
            std::env::set_var("KHIVE_ACTOR", "lambda:test-env-actor");
        }
        let base = RuntimeConfig::default();
        assert_eq!(base.actor_id.as_deref(), Some("lambda:test-env-actor"));

        let cfg = khive_cfg_with_actor("lambda:toml-actor");
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.actor_id.as_deref(),
            Some("lambda:toml-actor"),
            "TOML [actor] id must win over the env-resolved base.actor_id"
        );

        // SAFETY: restores prior KHIVE_ACTOR value (test cleanup).
        unsafe {
            match prior {
                Some(v) => std::env::set_var("KHIVE_ACTOR", v),
                None => std::env::remove_var("KHIVE_ACTOR"),
            }
        }
    }

    // ---- list_embedding_models tests ----

    // ---- core_backend accessor tests ----

    /// Create a migrated in-memory backend (for tests that need raw Arc<StorageBackend>).
    fn migrated_memory_backend() -> Arc<StorageBackend> {
        let backend = StorageBackend::memory().expect("memory backend");
        {
            let mut writer = backend.pool().try_writer().expect("writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }
        Arc::new(backend)
    }

    fn secondary_config() -> RuntimeConfig {
        RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::new("lore"),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        }
    }

    #[test]
    fn core_on_main_runtime_returns_same_backend_id() {
        // For a main-bound runtime, core() must return a clone with backend_id == "main".
        let rt = KhiveRuntime::memory().unwrap();
        assert_eq!(rt.backend_id().as_str(), BackendId::MAIN);
        let core_rt = rt.core();
        assert_eq!(core_rt.backend_id().as_str(), BackendId::MAIN);
    }

    #[tokio::test]
    async fn core_on_main_runtime_round_trips_note() {
        // core() on a main-bound runtime (core_backend = None) returns self.clone(),
        // so a note written through core() is readable through the original runtime.
        let rt = KhiveRuntime::memory().unwrap();
        let tok = NamespaceToken::local();

        let note = rt
            .core()
            .create_note(
                &tok,
                "observation",
                None,
                "adr073-main-round-trip",
                None,
                None,
                vec![],
            )
            .await
            .expect("create_note via core()");

        let found = rt
            .notes(&tok)
            .expect("notes store")
            .get_note(note.id)
            .await
            .expect("get_note");

        assert!(
            found.is_some(),
            "note written via core() must be visible through original rt"
        );
    }

    /// Proves note→main and aux→secondary writes are each isolated.
    ///
    /// Backend A = main; backend B = secondary.
    /// rt_secondary is bound to B with core_backend = Some(A).
    ///
    /// Direction 1 (note → main):
    ///   rt_secondary.core().create_note(...) must land in A (visible from rt_main)
    ///   and NOT in B (not visible from rt_secondary).
    ///
    /// Direction 2 (aux → secondary):
    ///   A raw SQL write via rt_secondary.sql() lands in B only; A is untouched.
    #[tokio::test]
    async fn cross_backend_split_note_to_main_aux_to_secondary() {
        use khive_storage::{SqlStatement, SqlValue};

        let main_arc = migrated_memory_backend();
        let secondary_arc = migrated_memory_backend();

        let main_config = RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        };

        let rt_main = KhiveRuntime::from_backend(main_arc.clone(), main_config);
        let rt_secondary = KhiveRuntime::from_backend(secondary_arc, secondary_config())
            .with_core_backend(main_arc.clone());

        let tok = NamespaceToken::local();

        // ── Direction 1: note must land in A (main), not in B (secondary) ──

        let note = rt_secondary
            .core()
            .create_note(
                &tok,
                "observation",
                None,
                "adr073-split-test",
                None,
                None,
                vec![],
            )
            .await
            .expect("create_note via core()");
        let note_id = note.id;

        // Visible from main (A).
        let in_main = rt_main
            .notes(&tok)
            .expect("main notes store")
            .get_note(note_id)
            .await
            .expect("get_note from main");
        assert!(
            in_main.is_some(),
            "note written via core() must appear in main backend A"
        );

        // Not visible from secondary (B).
        let in_secondary = rt_secondary
            .notes(&tok)
            .expect("secondary notes store")
            .get_note(note_id)
            .await
            .expect("get_note from secondary");
        assert!(
            in_secondary.is_none(),
            "note written to main via core() must NOT appear in secondary backend B"
        );

        // ── Direction 2: aux write via rt_secondary.sql() lands in B, not A ──

        {
            let mut writer = rt_secondary.sql().writer().await.expect("secondary writer");
            writer
                .execute(SqlStatement {
                    sql: "CREATE TABLE IF NOT EXISTS _test_adr073_aux \
                          (marker TEXT PRIMARY KEY)"
                        .into(),
                    params: vec![],
                    label: None,
                })
                .await
                .expect("create aux table in B");
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO _test_adr073_aux VALUES (?1)".into(),
                    params: vec![SqlValue::Text("b-side-sentinel".into())],
                    label: None,
                })
                .await
                .expect("insert into aux table in B");
        }

        // Row is present in B.
        let mut reader_b = rt_secondary.sql().reader().await.expect("secondary reader");
        let rows_b = reader_b
            .query_all(SqlStatement {
                sql: "SELECT marker FROM _test_adr073_aux".into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("select from B");
        assert_eq!(rows_b.len(), 1, "aux row must exist in B");
        match rows_b[0].get("marker") {
            Some(SqlValue::Text(s)) => {
                assert_eq!(s, "b-side-sentinel", "sentinel value must match")
            }
            other => panic!("expected Text('b-side-sentinel'), got {other:?}"),
        }

        // Row is absent from A (table does not exist there).
        let mut reader_a = rt_main.sql().reader().await.expect("main reader");
        let result_a = reader_a
            .query_all(SqlStatement {
                sql: "SELECT marker FROM _test_adr073_aux".into(),
                params: vec![],
                label: None,
            })
            .await;
        // A does not have this table → must error or return no rows.
        match result_a {
            Err(e) => assert!(
                e.to_string().contains("no such table"),
                "expected 'no such table' error from A, got: {e}"
            ),
            Ok(rows) => assert!(
                rows.is_empty(),
                "aux table must not have rows in A, got {} rows",
                rows.len()
            ),
        }
    }

    #[test]
    fn constructors_leave_core_backend_none_by_behavior() {
        // core() on any standard constructor returns a clone with same backend_id —
        // proof that core_backend = None (returns self.clone(), not a different backend).
        let rt_mem = KhiveRuntime::memory().unwrap();
        assert_eq!(rt_mem.core().backend_id().as_str(), BackendId::MAIN);

        let backend = migrated_memory_backend();
        let rt_from = KhiveRuntime::from_backend(
            backend,
            RuntimeConfig {
                git_write: Default::default(),
                db_path: None,
                default_namespace: Namespace::local(),
                embedding_model: None,
                additional_embedding_models: vec![],
                gate: Arc::new(AllowAllGate),
                packs: vec!["kg".to_string()],
                backend_id: BackendId::new("lore"),
                brain_profile: None,
                visible_namespaces: vec![],
                allowed_outbound_namespaces: vec![],
                actor_id: None,
            },
        );
        // from_backend with backend_id="lore" and no core_backend: core() returns
        // self.clone() which has backend_id="lore" (not "main").
        assert_eq!(rt_from.core().backend_id().as_str(), "lore");
    }

    #[test]
    fn with_core_backend_sets_core_then_core_returns_main_id() {
        // After wiring, core() must return a runtime with backend_id == "main".
        let main_arc = migrated_memory_backend();
        let secondary_arc = migrated_memory_backend();

        let rt_secondary = KhiveRuntime::from_backend(secondary_arc, secondary_config())
            .with_core_backend(main_arc);

        assert_eq!(rt_secondary.backend_id().as_str(), "lore");
        assert_eq!(
            rt_secondary.core().backend_id().as_str(),
            BackendId::MAIN,
            "core() on a secondary runtime must return a main-bound handle"
        );
    }

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
