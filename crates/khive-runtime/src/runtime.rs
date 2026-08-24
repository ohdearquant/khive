//! KhiveRuntime — composable handle to all storage capabilities.
//!
//! `RuntimeConfig`, `BackendId`, `NamespaceToken`, and embedding model helpers
//! live in `super::config` and are re-exported from here.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use khive_db::StorageBackend;
#[cfg(test)]
use khive_gate::AllowAllGate;
use khive_gate::GateRequest;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{
    AttachmentStore, EntityStore, Event, EventStore, GraphStore, NoteStore, SqlAccess, VectorStore,
};
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

/// Callback type for a pack-installed note-write validator.
///
/// The pack that owns a note kind carrying derivable identity installs one so
/// that the identity is a function of the authorization token rather than of
/// caller input, on every write path including direct callers that bypass the
/// handler layer — same rationale as [`EntityTypeValidatorFn`], which exists
/// for exactly that reason on the entity side.
///
/// Kinds the installing pack does not own must be returned unchanged: the slot
/// is single-occupancy (like `note_mutation_hook`), so a validator that
/// rewrote foreign kinds would silently govern every other pack's notes.
pub type NoteWriteValidatorFn = Arc<
    dyn Fn(&str, &str, Option<serde_json::Value>) -> Result<Option<serde_json::Value>, RuntimeError>
        + Send
        + Sync,
>;

/// Immutable identity for a non-text vector store owned by a pack consumer.
///
/// This does not register an [`crate::EmbedderProvider`]. It gives a pack that
/// performs its own governed inference a narrow path to a namespace-scoped
/// Khive vector table while keeping model-key and dimension validation at the
/// runtime boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NamedVectorIdentity {
    model_key: String,
    model_name: String,
    dimensions: usize,
}

impl NamedVectorIdentity {
    const MAX_MODEL_KEY_BYTES: usize = 128;
    const MAX_MODEL_NAME_BYTES: usize = 512;

    /// Validate and construct a named vector identity.
    pub fn new(
        model_key: impl Into<String>,
        model_name: impl Into<String>,
        dimensions: usize,
    ) -> RuntimeResult<Self> {
        let model_key = model_key.into();
        let model_name = model_name.into();
        if model_key.is_empty()
            || model_key.len() > Self::MAX_MODEL_KEY_BYTES
            || !model_key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(RuntimeError::InvalidInput(format!(
                "named vector model_key must be 1..={} bytes of ASCII alphanumeric/underscore",
                Self::MAX_MODEL_KEY_BYTES
            )));
        }
        if model_name.trim().is_empty()
            || model_name.trim() != model_name
            || model_name.len() > Self::MAX_MODEL_NAME_BYTES
        {
            return Err(RuntimeError::InvalidInput(format!(
                "named vector model_name must be 1..={} bytes with no surrounding whitespace",
                Self::MAX_MODEL_NAME_BYTES
            )));
        }
        if !(1..=8192).contains(&dimensions) {
            return Err(RuntimeError::InvalidInput(format!(
                "named vector dimensions must be in 1..=8192, got {dimensions}"
            )));
        }
        Ok(Self {
            model_key,
            model_name,
            dimensions,
        })
    }

    pub fn model_key(&self) -> &str {
        &self.model_key
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
}

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
    /// ADR-118 exact-leg policy, sampled once at runtime construction.
    /// Request-time memory/knowledge serving must never re-read the process
    /// environment because tests and embedded runtimes share one process.
    ann_fresh_tail_enabled: bool,
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
    /// Pack-installed note-write validator.
    ///
    /// When `Some`, every runtime note-materialisation site that accepts
    /// caller-supplied `properties` routes them through this function before
    /// the `Note` is built, so a pack-owned identity property is derived from
    /// the authorization token instead of trusted from caller input. `None`
    /// on a bare runtime (no packs) — the properties pass through unchanged.
    note_write_validator: Arc<RwLock<Option<NoteWriteValidatorFn>>>,
    /// Pack-owned note kinds — every note kind declared by a pack other than
    /// the generic-CRUD pack, installed by the transport from the registry
    /// (see `VerbRegistry::pack_owned_note_kinds`). Records of these kinds are
    /// maintained by their owning pack's own verbs, so `update`'s `properties`
    /// patch is refused on them at the runtime layer and their owned identity
    /// properties survive a `merge` unchanged. Empty until installed (bare
    /// runtime), which leaves both rules inert.
    pack_owned_note_kinds: Arc<RwLock<Vec<String>>>,
    /// The immutable runtime-owned store/hydration pair (ADR-160 D3).
    ///
    /// Boot resolves one store and constructs exactly one shared hydrator,
    /// then installs that same `Arc` on every runtime handle. The one-shot
    /// slot rejects replacement so pack runtimes cannot silently split the
    /// aggregate byte budget after startup. Bare runtimes leave it unset.
    blob_hydrator: Arc<OnceLock<Arc<crate::blob::BlobHydrator>>>,
    /// Pack-registered custom fusion executors (ADR-012), keyed by the name
    /// carried in `FusionStrategy::Custom { name, .. }`.
    ///
    /// Unlike `entity_type_validator`/`note_mutation_hook` (single-occupancy —
    /// one pack owns the slot), multiple packs each register their own named
    /// strategy under this shared map, so it is keyed rather than a bare
    /// `Option`. Empty until a pack calls
    /// [`register_fusion_strategy`](Self::register_fusion_strategy); an
    /// unregistered `Custom` name at dispatch time is
    /// `RuntimeError::UnknownFusionStrategy`, never a silent fallback.
    fusion_executors: Arc<RwLock<HashMap<String, Arc<dyn crate::fusion::FusionExecutor>>>>,
}

impl KhiveRuntime {
    /// Create a new runtime with the given config.
    ///
    /// The config's `db_path` is used to open or create the SQLite backend.
    /// This direct constructor is intended for fresh/current single-backend
    /// databases and tests. Production and multi-backend hosts must use the
    /// async khive-mcp/kkernel builders so secondary inventory and any
    /// application-assisted V21 cutover complete before serving. The
    /// [`from_backend`](Self::from_backend) seam is likewise only for an
    /// already-prepared backend.
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
        // Writable backends migrate before handlers touch the DB. A detected
        // read-only snapshot is validated at the current schema version without
        // attempting migration DDL.
        let schema_version = backend.prepare_core_schema()?;
        if schema_version < khive_db::migrations::ATTACHMENT_CUTOVER_VERSION {
            return Err(khive_db::SqliteError::InvalidData(
                "database requires the host application-assisted V21 attachment cutover; \
                 start through khive-mcp/kkernel boot instead of constructing KhiveRuntime \
                 directly"
                    .into(),
            )
            .into());
        }
        if !backend.is_read_only() {
            register_configured_embedding_models(&backend, &config)?;
        }
        Ok(Self::assemble_from_backend(Arc::new(backend), config))
    }

    /// Open a runtime for read-only inspection (no model registration, no DB creation).
    ///
    /// File-backed databases are opened with SQLite read-only/query-only flags
    /// and must already be at this build's current schema version. No migrations
    /// or configured-model registration writes are attempted. A `None` path
    /// retains the historical ephemeral in-memory behavior for tests.
    pub fn new_readonly(config: RuntimeConfig) -> RuntimeResult<Self> {
        let backend = match &config.db_path {
            Some(path) => StorageBackend::sqlite_read_only(path)?,
            None => StorageBackend::memory()?,
        };
        backend.prepare_core_schema()?;
        Ok(Self::assemble_from_backend(Arc::new(backend), config))
    }

    /// Construct a runtime from an already-opened backend.
    ///
    /// This is a low-level, infallible assembly seam for already-prepared
    /// multi-backend deployments. It does not inspect or migrate the V21
    /// attachment-cutover state. Production hosts must first run the async
    /// kkernel/khive-mcp coordinator and must not expose a server over a
    /// pending or incomplete backend. Prefer [`Self::from_prepared_backend`]
    /// when constructing one fallible host runtime.
    ///
    /// The returned runtime has `db_path = None` and `embedding_model = None`; all
    /// storage access is through the provided `backend`. Set `backend_id` and
    /// `default_namespace` via the config builder pattern if non-defaults are needed.
    pub fn from_backend(backend: Arc<StorageBackend>, config: RuntimeConfig) -> Self {
        if !backend.is_read_only() {
            if let Err(err) = register_configured_embedding_models(&backend, &config) {
                tracing::warn!(error = %err, "failed to register configured embedding models");
            }
        }
        Self::assemble_from_backend(backend, config)
    }

    /// Construct a single-backend runtime after a host boot coordinator has
    /// completed schema preparation and any application-assisted cutover.
    ///
    /// Unlike [`Self::from_backend`], configured embedding-model registration
    /// is fallible here, preserving [`Self::new`]'s single-backend startup
    /// semantics. This method never runs migrations itself.
    pub fn from_prepared_backend(
        backend: Arc<StorageBackend>,
        config: RuntimeConfig,
    ) -> RuntimeResult<Self> {
        if backend.attachment_cutover_status()?
            != khive_db::migrations::AttachmentCutoverStatus::Complete
        {
            return Err(khive_db::SqliteError::InvalidData(
                "from_prepared_backend requires a complete V21 attachment cutover".into(),
            )
            .into());
        }
        if !backend.is_read_only() {
            register_configured_embedding_models(&backend, &config)?;
        }
        Ok(Self::assemble_from_backend(backend, config))
    }

    fn assemble_from_backend(backend: Arc<StorageBackend>, config: RuntimeConfig) -> Self {
        let ann_fresh_tail_enabled = crate::config::ann_fresh_tail_enabled_from_env();
        let (registry, default_embedder_name) = build_embedder_registry(&config);
        Self {
            backend,
            core_backend: None,
            config,
            ann_fresh_tail_enabled,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
            entity_type_validator: Arc::new(RwLock::new(None)),
            note_mutation_hook: Arc::new(RwLock::new(None)),
            note_write_validator: Arc::new(RwLock::new(None)),
            pack_owned_note_kinds: Arc::new(RwLock::new(Vec::new())),
            blob_hydrator: Arc::new(OnceLock::new()),
            fusion_executors: Arc::new(RwLock::new(HashMap::new())),
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
                    ann_fresh_tail_enabled: self.ann_fresh_tail_enabled,
                    embedder_registry: self.embedder_registry.clone(),
                    default_embedder_name: self.default_embedder_name.clone(),
                    edge_rules: self.edge_rules.clone(),
                    valid_entity_kinds: self.valid_entity_kinds.clone(),
                    valid_note_kinds: self.valid_note_kinds.clone(),
                    entity_type_validator: self.entity_type_validator.clone(),
                    note_mutation_hook: self.note_mutation_hook.clone(),
                    note_write_validator: self.note_write_validator.clone(),
                    pack_owned_note_kinds: self.pack_owned_note_kinds.clone(),
                    blob_hydrator: self.blob_hydrator.clone(),
                    fusion_executors: self.fusion_executors.clone(),
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

    /// Return the immutable ADR-118 fresh-tail serving policy captured when
    /// this runtime was constructed.
    pub fn ann_fresh_tail_enabled(&self) -> bool {
        self.ann_fresh_tail_enabled
    }

    /// Override ADR-118's fresh-tail serving policy for this runtime instance.
    ///
    /// This is primarily useful for embedded runtimes and deterministic tests:
    /// it avoids mutating process-global environment state. Clones and `core()`
    /// handles preserve the chosen value.
    pub fn with_ann_fresh_tail_enabled(mut self, enabled: bool) -> Self {
        self.ann_fresh_tail_enabled = enabled;
        self
    }

    /// Return a reference to the underlying storage backend.
    ///
    /// This is an embedder/infrastructure surface (connection pools, schema
    /// plans, diagnostics). Stores obtained from it are NOT wrapped by the
    /// message-evidence policy that [`Self::notes`] enforces: an embedder
    /// holding the backend already holds root-equivalent access to the
    /// database file, so the policy boundary sits at the typed accessors
    /// pack code uses, not here. Pack code must not take note stores from
    /// this surface.
    pub fn backend(&self) -> &StorageBackend {
        &self.backend
    }

    /// Whether this runtime's bound backend is explicitly or filesystem-mode
    /// detected read-only.
    pub fn is_read_only(&self) -> bool {
        self.backend.is_read_only()
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

    /// Writer-contention, graph-edge integrity, and WAL/checkpoint diagnostics
    /// (ADR-091/ADR-135 operator surface): pooled writer and audit-failure
    /// counters, build identity, duplicate edge-ID and list-ledger counts,
    /// checkpoint counters, a PASSIVE checkpoint probe, WAL file size, and
    /// explicitly qualified WAL-pin census. Not write-free: the
    /// PASSIVE probe may backfill WAL frames into the database (normal
    /// checkpoint I/O). It never changes logical state, escalates to TRUNCATE,
    /// creates a missing database file, or deletes sidecar evidence — see
    /// `khive_db::diagnostics` for the narrowings that make those claims hold.
    ///
    /// Always targets the *main* backend via [`Self::core`], regardless of
    /// which backend this runtime handle is bound to, so a report never
    /// describes a database this handle is not the canonical owner of.
    pub async fn db_diagnostics(&self) -> RuntimeResult<khive_db::diagnostics::DbDiagnostics> {
        // No `VerbRegistry` handle is reachable from a bare `KhiveRuntime`
        // (the audit-batch seam is owned by whichever registry was built
        // over this runtime's `EventStore`, not by the runtime itself), so
        // the batch-health fields report unavailable with a reason here.
        // Callers that hold the registry — e.g. the `db_diagnostics` verb
        // handler — use `Self::db_diagnostics_with_audit_metrics` with
        // `VerbRegistry::audit_batch_metrics()` instead.
        self.db_diagnostics_with_audit_metrics(None).await
    }

    /// As [`Self::db_diagnostics`], but with the caller supplying the
    /// ADR-133 audit-batch health counters from whichever `VerbRegistry`
    /// owns the seam over this runtime's `EventStore` (typically
    /// `VerbRegistry::audit_batch_metrics()`). `None` behaves identically to
    /// [`Self::db_diagnostics`].
    pub async fn db_diagnostics_with_audit_metrics(
        &self,
        runtime_audit_batch_metrics: Option<khive_db::diagnostics::RuntimeAuditBatchMetrics>,
    ) -> RuntimeResult<khive_db::diagnostics::DbDiagnostics> {
        let pool = self.core().backend.pool_arc();
        let interval = khive_db::CheckpointConfig::from_env().interval;
        let build_hash = crate::build_info::BUILD_INFO
            .is_stamped()
            .then_some(crate::build_info::BUILD_INFO.source_revision);
        let build =
            khive_db::diagnostics::BuildIdentity::from_env(env!("CARGO_PKG_VERSION"), build_hash);

        khive_db::diagnostics::collect_with_runtime_audit_metrics_interruptibly(
            pool,
            build,
            interval,
            crate::pack::audit_append_failure_count(),
            runtime_audit_batch_metrics,
        )
        .await
        .map_err(RuntimeError::from)
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
    ///
    /// Wrapped in `note_store_guard::PolicyEnforcingNoteStore`, which
    /// refuses any insert/upsert of a `kind = "message"` note carrying
    /// `quarantined` / `channel_kind` / `channel_slug` — the transport-owned
    /// evidence `comm.health` trusts at face value — and refuses patching
    /// those keys through the property-mutation seams on any note kind, so
    /// the guard cannot be sidestepped by inserting a clean message note and
    /// patching the evidence onto it afterward. The trusted channel-ingest
    /// path does not go through this accessor; see
    /// `Self::raw_notes` and [`Self::try_create_note_as_trusted_ingest`].
    pub fn notes(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn NoteStore>> {
        Ok(crate::note_store_guard::PolicyEnforcingNoteStore::wrap(
            self.raw_notes(token)?,
        ))
    }

    /// Get the unwrapped, policy-free NoteStore scoped to the token's namespace.
    ///
    /// Bypasses `note_store_guard::PolicyEnforcingNoteStore`. Callers
    /// within this crate that have already enforced the reserved-transport-
    /// property policy themselves (namely `try_create_note_impl`, which
    /// applies it conditionally based on whether the caller presented a
    /// [`crate::pack::ChannelIngestCapability`]) use this to reach storage
    /// directly rather than run a redundant, less-informed check. Not exposed
    /// outside this crate — every other caller must use [`Self::notes`].
    pub(crate) fn raw_notes(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn NoteStore>> {
        Ok(self
            .backend
            .notes_for_namespace(token.namespace().as_str())?)
    }

    /// Return the role-keyed attachment substrate on the canonical main backend.
    ///
    /// Attachment rows are the process-shared BlobStore's sole SQL liveness
    /// authority. A runtime bound directly to a secondary pack backend must call
    /// [`Self::core`] first; accepting a secondary mutation here would create a
    /// reference that the main-database GC sweep cannot see or fence.
    pub fn attachments(&self) -> RuntimeResult<Arc<dyn AttachmentStore>> {
        if self.config.backend_id.as_str() != BackendId::MAIN {
            return Err(RuntimeError::InvalidInput(format!(
                "attachments are owned by the canonical main backend; runtime backend {:?} must route through KhiveRuntime::core()",
                self.config.backend_id.as_str()
            )));
        }
        Ok(self.backend.attachments()?)
    }

    /// Get an EventStore scoped to the token's namespace.
    ///
    /// When the events-daemon split (ADR-170) is configured, the store routes
    /// by append class: the ADR-133 idempotent audit-batch lane — the
    /// measured bulk of event write volume — persists to `events.db`
    /// (forwarded over the events daemon socket in daemon deployments, or
    /// opened directly in embedded/one-shot contexts), while plain appends
    /// stay on this runtime's backend, keeping every raw-SQL consumer of the
    /// legacy `events` table (schedule provenance, kg projection guards,
    /// GraphQuery's substrate union) correct by construction. Reads merge
    /// both stores. Unconfigured runtimes (tests, in-memory) keep the legacy
    /// main-store behavior.
    pub fn events(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn EventStore>> {
        let legacy = self
            .backend
            .events_for_namespace(token.namespace().as_str())?;
        match &self.config.events_split {
            None => Ok(legacy),
            Some(split) => {
                let lane: Arc<dyn EventStore> = match &split.socket_path {
                    Some(socket) => {
                        let client = crate::events_split::client_for(socket)?;
                        Arc::new(crate::events_split::ForwardingEventStore::new(
                            token.namespace().as_str(),
                            client,
                        ))
                    }
                    None => crate::events_split::direct_backend_for(&split.db_path)?
                        .events_for_namespace(token.namespace().as_str())?,
                };
                Ok(Arc::new(crate::events_split::SplitEventStore::new(
                    legacy, lane,
                )))
            }
        }
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

    /// Get a namespace-scoped vector store for a pack-owned immutable identity.
    ///
    /// The table key is syntactically validated by [`NamedVectorIdentity`]. This
    /// accessor additionally verifies the table's actual sqlite-vec dimension
    /// declaration and every persisted `embedding_model` value before returning
    /// the store, so reusing one key for incompatible descriptor geometry or
    /// semantics fails before a caller can replace rows.
    pub async fn vectors_for_named_identity(
        &self,
        token: &NamespaceToken,
        identity: &NamedVectorIdentity,
    ) -> RuntimeResult<Arc<dyn VectorStore>> {
        let store = self.backend.vectors_for_namespace(
            identity.model_key(),
            identity.model_name(),
            identity.dimensions(),
            token.namespace().as_str(),
        )?;

        let table = format!("vec_{}", identity.model_key());
        let mut reader = self.sql().reader().await?;
        let dimension_row = reader
            .query_row(SqlStatement {
                sql: "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1".to_string(),
                params: vec![SqlValue::Text(table.clone())],
                label: Some("runtime_named_vector_dimension".to_string()),
            })
            .await?
            .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "named vector table {table} has no sqlite_schema declaration"
                ))
            })?;
        let table_ddl = match dimension_row.get("sql") {
            Some(SqlValue::Text(value)) => value,
            other => {
                return Err(RuntimeError::Internal(format!(
                    "named vector table {table} returned invalid schema metadata: {other:?}"
                )))
            }
        };
        let declared_dimensions = vector_dimensions_from_ddl(table_ddl).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "named vector table {table} has no parseable embedding dimension"
            ))
        })?;
        if declared_dimensions != identity.dimensions() {
            return Err(RuntimeError::InvalidInput(format!(
                "named vector model_key {:?} is already bound to {declared_dimensions} dimensions, expected {}",
                identity.model_key(),
                identity.dimensions()
            )));
        }

        let stored_models = reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT DISTINCT embedding_model FROM {table} ORDER BY embedding_model LIMIT 2"
                ),
                params: vec![],
                label: Some("runtime_named_vector_model_identity".to_string()),
            })
            .await?;
        for row in stored_models {
            let stored = match row.get("embedding_model") {
                Some(SqlValue::Text(value)) => value,
                other => {
                    return Err(RuntimeError::Internal(format!(
                        "named vector table {table} returned invalid model identity metadata: {other:?}"
                    )))
                }
            };
            if stored != identity.model_name() {
                return Err(RuntimeError::InvalidInput(format!(
                    "named vector model_key {:?} already contains model {stored:?}, cannot bind it to {:?}",
                    identity.model_key(),
                    identity.model_name()
                )));
            }
        }

        self.backend
            .register_embedding_model(
                identity.model_key(),
                identity.model_name(),
                identity.model_key(),
                identity.dimensions() as u32,
            )
            .map_err(|error| {
                if matches!(
                    &error,
                    khive_db::SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
                        if code.code == rusqlite::ErrorCode::ConstraintViolation
                ) {
                    RuntimeError::InvalidInput(format!(
                        "named vector model_key {:?} is already bound to a different active model identity",
                        identity.model_key()
                    ))
                } else {
                    RuntimeError::Sqlite(error)
                }
            })?;

        Ok(store)
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
            Err(e) => {
                tracing::warn!(
                    namespace = %ns.as_str(),
                    error = %crate::secret_gate::bounded_masked_log_text(&e.to_string()),
                    "authorize: gate check failed (fail-closed)"
                );
                Err(crate::RuntimeError::Internal(format!(
                    "gate error: {}",
                    e.wire_reason()
                )))
            }
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
            Err(e) => {
                tracing::warn!(
                    namespace = %primary.as_str(),
                    error = %crate::secret_gate::bounded_masked_log_text(&e.to_string()),
                    "authorize_with_visibility: gate check failed (fail-closed)"
                );
                Err(crate::RuntimeError::Internal(format!(
                    "gate error: {}",
                    e.wire_reason()
                )))
            }
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

    /// Install an already-paired blob hydrator into this runtime.
    ///
    /// Reinstalling the exact same `Arc` is idempotent. A different pair is
    /// rejected: replacing it would split or reset the aggregate admission
    /// budget while requests may still hold leases.
    pub fn install_blob_hydrator(
        &self,
        hydrator: Arc<crate::blob::BlobHydrator>,
    ) -> RuntimeResult<()> {
        if hydrator.budget_bytes() != self.config.blob_hydration_bytes {
            return Err(RuntimeError::InvalidInput(format!(
                "blob hydrator budget {} does not match this runtime's resolved budget {}",
                hydrator.budget_bytes(),
                self.config.blob_hydration_bytes
            )));
        }
        if let Some(current) = self.blob_hydrator.get() {
            return if Arc::ptr_eq(current, &hydrator) {
                Ok(())
            } else {
                Err(RuntimeError::InvalidInput(
                    "a different blob hydrator is already installed".to_string(),
                ))
            };
        }

        match self.blob_hydrator.set(hydrator) {
            Ok(()) => Ok(()),
            Err(candidate) => {
                let current = self.blob_hydrator.get().ok_or_else(|| {
                    RuntimeError::Internal(
                        "blob hydrator install raced without a visible winner".to_string(),
                    )
                })?;
                if Arc::ptr_eq(current, &candidate) {
                    Ok(())
                } else {
                    Err(RuntimeError::InvalidInput(
                        "a different blob hydrator is already installed".to_string(),
                    ))
                }
            }
        }
    }

    /// Pair and install a store using this runtime's resolved hydration budget.
    ///
    /// Boot paths that own multiple runtimes should instead construct one
    /// [`crate::BlobHydrator`] and call [`Self::install_blob_hydrator`] with
    /// the same `Arc` on every handle.
    pub fn install_blob_store(
        &self,
        store: Arc<dyn khive_storage::BlobStore>,
    ) -> RuntimeResult<()> {
        if let Some(current) = self.blob_hydrator.get() {
            let current_store = current.store();
            if Arc::ptr_eq(&current_store, &store) {
                return Ok(());
            }
        }
        let hydrator = Arc::new(crate::blob::BlobHydrator::new(
            store,
            self.config.blob_hydration_bytes,
        )?);
        self.install_blob_hydrator(hydrator)
    }

    /// Return the installed shared blob hydrator, if boot configured one.
    pub fn blob_hydrator(&self) -> Option<Arc<crate::blob::BlobHydrator>> {
        self.blob_hydrator.get().cloned()
    }

    /// Return the installed `BlobStore`, if the boot path resolved and
    /// installed one. `None` when no `[storage.blob]` selection was ever
    /// installed — e.g. a bare/test runtime constructed without going
    /// through the `khive-mcp` boot path.
    pub fn blob_store(&self) -> Option<Arc<dyn khive_storage::BlobStore>> {
        self.blob_hydrator.get().map(|hydrator| hydrator.store())
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

    /// Install the pack-owned note kinds aggregated from the pack registry.
    ///
    /// Called by the transport after the `VerbRegistry` is built, same timing
    /// as [`install_kind_registry`](Self::install_kind_registry).
    pub fn install_pack_owned_note_kinds(&self, kinds: Vec<String>) {
        if let Ok(mut guard) = self.pack_owned_note_kinds.write() {
            *guard = kinds;
        }
    }

    /// Whether `kind` is a note kind owned by a pack (see
    /// [`install_pack_owned_note_kinds`](Self::install_pack_owned_note_kinds)).
    ///
    /// Always `false` before the transport installs the list — a bare runtime
    /// has no packs, so no kind is pack-owned there.
    pub fn is_pack_owned_note_kind(&self, kind: &str) -> bool {
        self.pack_owned_note_kinds
            .read()
            .map(|g| g.iter().any(|k| k == kind))
            .unwrap_or(false)
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

    /// Install a pack-owned note-write validator.
    ///
    /// Called during pack registration (`PackRuntime::register_note_write_validator`)
    /// so that the covered note-write sites carrying caller-supplied
    /// `properties` derive the owning pack's identity properties from the
    /// authorization token, closing the gap where a direct Rust caller, the
    /// generic `create` verb, or the proposal-apply path (which dispatches no
    /// pack hooks) writes them unchecked. Single-slot semantics, same as
    /// [`install_note_mutation_hook`](Self::install_note_mutation_hook): a
    /// second installing pack overwrites the first, so a validator must return
    /// kinds it does not own unchanged.
    ///
    /// Covered sites — each calls `derive_note_write_properties`
    /// before the write: `create_note_inner` (`operations.rs`, the generic
    /// `create` verb funnel and every other public `create_note*` variant),
    /// `atomic_prepare::prepare_add_note` (the proposal-apply add-note path),
    /// and `atomic_message::create_notes_atomic_with_report` (the atomic
    /// multi-note writer).
    ///
    /// NOT covered by this validator: `try_create_note` (`operations.rs`).
    /// `try_create_note` is deliberately excluded — its only caller path is
    /// `comm.ingest`, where `properties.from_actor` is the external transport
    /// sender named by the `from` parameter, not the authenticated caller,
    /// and where transport-owned quarantine/channel properties are
    /// legitimately established. Running the generic validator there would
    /// stamp every inbound message as the ingesting daemon and reject the
    /// evidence the trusted ingest handler just derived. `try_create_note`
    /// instead runs its own narrower reserved-transport-property check
    /// inline (`operations.rs`'s `try_create_note_impl`), which allows the
    /// three `message`-kind transport properties only when called through
    /// [`Self::try_create_note_as_trusted_ingest`] with a
    /// [`crate::pack::ChannelIngestCapability`].
    ///
    /// The `NoteStore` returned by [`notes`](Self::notes) is covered by a
    /// different, narrower mechanism: it is wrapped in
    /// `note_store_guard::PolicyEnforcingNoteStore`, which refuses
    /// `upsert_note` / `upsert_notes` / `try_insert_note` /
    /// `replace_note_if_unchanged` calls that would write a `kind = "message"`
    /// note carrying `quarantined` / `channel_kind` / `channel_slug`, and
    /// refuses `set_note_property` / `try_patch_note_property` /
    /// `patch_note_property_atomic` / `update_note_properties` calls that
    /// would patch any of those keys onto any note — unconditionally, since
    /// that public accessor has no way to see a trust decision. `try_create_note_impl` itself reaches storage through
    /// `Self::raw_notes`, the unwrapped accessor, so its own inline check
    /// (which can legitimately allow those properties for trusted ingest)
    /// is not double-enforced or contradicted by the wrapper.
    /// Register a pack-defined custom fusion strategy under `name` (ADR-012).
    ///
    /// Unlike `install_entity_type_validator`/`install_note_mutation_hook`,
    /// this slot is keyed rather than single-occupancy: multiple packs each
    /// register their own named strategy, and a second registration under an
    /// already-used `name` replaces the first. Looked up by
    /// `FusionStrategy::Custom { name, .. }` at the hybrid-search dispatch
    /// boundary in `crate::fusion`; an unregistered name fails closed with
    /// `RuntimeError::UnknownFusionStrategy` rather than silently falling
    /// back to RRF.
    pub fn register_fusion_strategy(
        &self,
        name: impl Into<String>,
        executor: Arc<dyn crate::fusion::FusionExecutor>,
    ) {
        if let Ok(mut guard) = self.fusion_executors.write() {
            guard.insert(name.into(), executor);
        }
    }

    /// Resolve a registered custom fusion executor by name.
    ///
    /// Returns `RuntimeError::UnknownFusionStrategy` when no pack has
    /// registered `name` — callers must invoke this before any
    /// empty-input/zero-limit short circuit so a misconfigured name errors
    /// on every call, including zero-result ones.
    pub(crate) fn fusion_executor(
        &self,
        name: &str,
    ) -> RuntimeResult<Arc<dyn crate::fusion::FusionExecutor>> {
        let guard = self
            .fusion_executors
            .read()
            .map_err(|_| RuntimeError::Internal("fusion executor registry lock poisoned".into()))?;
        guard
            .get(name)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownFusionStrategy(name.to_string()))
    }

    pub fn install_note_write_validator(&self, f: NoteWriteValidatorFn) {
        if let Ok(mut guard) = self.note_write_validator.write() {
            *guard = Some(f);
        }
    }

    /// Whether a note-write validator is installed on this runtime.
    ///
    /// Exists so a transport's own tests can assert, per boot path, that the
    /// documented startup sequence actually filled the slot. A missing install
    /// fails open and silently — an empty slot passes caller-supplied
    /// properties straight through, which no write site can distinguish from a
    /// validator that approved them — so occupancy is asserted, never assumed.
    pub fn has_note_write_validator(&self) -> bool {
        self.note_write_validator
            .read()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    /// Run caller-supplied note `properties` through the installed note-write
    /// validator, returning the properties to store.
    ///
    /// Returns them unchanged when no validator is installed (bare runtime).
    pub(crate) fn derive_note_write_properties(
        &self,
        kind: &str,
        token: &NamespaceToken,
        properties: Option<serde_json::Value>,
    ) -> RuntimeResult<Option<serde_json::Value>> {
        let validator = self
            .note_write_validator
            .read()
            .map_err(|_| RuntimeError::Internal("note write validator lock poisoned".into()))?
            .clone();
        match validator {
            None => Ok(properties),
            Some(validate) => validate(kind, &token.actor().id, properties),
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
        // Lazy embedder construction can happen during daemon warm or an
        // assertive request. A snapshot has no durable audit sink, so do not
        // resolve an EventStore merely to attempt a known-rejected append.
        if self.is_read_only() {
            return;
        }
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

fn vector_dimensions_from_ddl(ddl: &str) -> Option<usize> {
    let lower = ddl.to_ascii_lowercase();
    let suffix = lower.split_once("embedding float[")?.1;
    let dimension = suffix.split_once(']')?.0;
    if dimension.is_empty() || !dimension.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    dimension.parse().ok()
}

// INLINE TEST JUSTIFICATION: tests here cover KhiveRuntime construction helpers
// (in-memory backend wiring, NamespaceToken::for_namespace) that are
// pub(crate)-only and cannot be called from the integration test crate.
#[cfg(test)]
mod tests {
    use super::*;
    use khive_gate::GateRef;
    use serial_test::serial;

    fn test_blob_hydrator() -> (tempfile::TempDir, Arc<crate::BlobHydrator>) {
        let root = tempfile::tempdir().expect("blob root");
        let store = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0)
                .expect("fs blob store"),
        );
        let hydrator = Arc::new(
            crate::BlobHydrator::new(store, crate::DEFAULT_BLOB_HYDRATION_BYTES)
                .expect("blob hydrator"),
        );
        (root, hydrator)
    }

    #[test]
    fn memory_runtime_creates_successfully() {
        let rt = KhiveRuntime::memory().expect("memory runtime should create");
        assert!(rt.config().db_path.is_none());
    }

    #[test]
    fn installed_blob_hydrator_is_shared_by_clone_and_core_handles() {
        let main_backend = Arc::new(StorageBackend::memory().expect("main backend"));
        let pack_backend = Arc::new(StorageBackend::memory().expect("pack backend"));
        let mut config = RuntimeConfig::no_embeddings();
        config.backend_id = BackendId::new("assets");
        let runtime = KhiveRuntime::from_backend(pack_backend, config)
            .with_core_backend(Arc::clone(&main_backend));
        let (_root, hydrator) = test_blob_hydrator();

        runtime
            .install_blob_hydrator(Arc::clone(&hydrator))
            .expect("first install");

        for handle in [runtime.clone(), runtime.core()] {
            let installed = handle.blob_hydrator().expect("installed hydrator");
            assert!(Arc::ptr_eq(&installed, &hydrator));
        }
    }

    #[test]
    fn blob_hydrator_install_is_idempotent_but_rejects_replacement() {
        let runtime = KhiveRuntime::memory().expect("runtime");
        let (_first_root, first) = test_blob_hydrator();
        let (_second_root, second) = test_blob_hydrator();

        runtime
            .install_blob_hydrator(Arc::clone(&first))
            .expect("first install");
        runtime
            .install_blob_hydrator(Arc::clone(&first))
            .expect("same Arc reinstall is idempotent");

        let error = runtime
            .install_blob_hydrator(second)
            .expect_err("a different hydrator must not replace the installed pair");
        assert!(error.to_string().contains("already installed"));
        assert!(Arc::ptr_eq(
            &runtime.blob_hydrator().expect("original remains"),
            &first
        ));
    }

    #[test]
    fn blob_hydrator_install_rejects_a_budget_that_disagrees_with_runtime_identity() {
        let runtime = KhiveRuntime::memory().expect("runtime");
        let root = tempfile::tempdir().expect("blob root");
        let store = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0)
                .expect("fs blob store"),
        );
        let mismatched = Arc::new(
            crate::BlobHydrator::new(store, khive_storage::MAX_BLOB_WHOLE_BYTES)
                .expect("minimum blob budget"),
        );

        let error = runtime
            .install_blob_hydrator(mismatched)
            .expect_err("live admission must match the construction-baked config identity");
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(runtime.blob_hydrator().is_none());
    }

    #[test]
    fn fresh_tail_policy_is_instance_scoped_and_clone_stable() {
        let enabled = KhiveRuntime::memory()
            .expect("enabled memory runtime")
            .with_ann_fresh_tail_enabled(true);
        let disabled = KhiveRuntime::memory()
            .expect("disabled memory runtime")
            .with_ann_fresh_tail_enabled(false);

        assert!(enabled.ann_fresh_tail_enabled());
        assert!(enabled.clone().ann_fresh_tail_enabled());
        assert!(!disabled.ann_fresh_tail_enabled());
        assert!(!disabled.clone().ann_fresh_tail_enabled());
    }

    #[tokio::test]
    async fn runtime_db_diagnostics_supplies_both_contention_counter_sources() {
        let rt = KhiveRuntime::memory().expect("memory runtime should create");

        let report = rt.db_diagnostics().await.expect("diagnostics succeed");

        assert!(
            report.writer_contention.writer_acquisitions >= 1,
            "runtime construction runs migrations through the finite-wait pooled writer"
        );
        assert_eq!(
            report.writer_contention.writer_acquisitions,
            report
                .writer_contention
                .pooled_writer_acquisitions
                .saturating_add(report.writer_contention.standalone_writer_acquisitions)
                .saturating_add(report.writer_contention.writer_task_acquisitions),
            "the public aggregate must equal the class-specific snapshot"
        );
        assert!(
            report.writer_contention.audit_append_failures.is_some(),
            "the runtime path must supply its process-wide swallowed-audit counter"
        );
        assert!(report
            .writer_contention
            .audit_append_failures_unavailable_reason
            .is_none());
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(path),
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(path.clone()),
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_boot_detects_read_only_snapshot_and_skips_model_registration() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read_only_runtime.db");
        let base = RuntimeConfig {
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(path.clone()),
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
        {
            let writable = KhiveRuntime::new(base.clone()).expect("create migrated snapshot");
            assert!(writable
                .list_embedding_models(None)
                .await
                .expect("registry query")
                .is_empty());
        }

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&path, permissions).unwrap();
        // A lingering writable `-shm` from the writable fixture's asynchronous
        // connection close is rejected by read-only admission as potentially
        // live; freeze any sidecars into the documented frozen-snapshot form.
        for suffix in ["-wal", "-shm"] {
            let mut name = path.file_name().expect("db file name").to_os_string();
            name.push(suffix);
            let sidecar = path.parent().expect("db parent dir").join(name);
            if sidecar.exists() {
                let mut sidecar_permissions = std::fs::metadata(&sidecar)
                    .expect("sidecar metadata")
                    .permissions();
                sidecar_permissions.set_mode(0o444);
                std::fs::set_permissions(&sidecar, sidecar_permissions).expect("freeze sidecar");
            }
        }

        let read_only_config = RuntimeConfig {
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            ..base
        };
        let runtime = KhiveRuntime::new(read_only_config)
            .expect("read-only boot must validate instead of migrating/registering");
        assert!(runtime.is_read_only());
        assert_eq!(
            runtime.backend().pool().writer_acquisition_snapshot(),
            khive_db::pool::WriterAcquisitionSnapshot::default(),
            "the construction-inclusive acquisition baseline must stay at zero"
        );
        assert!(
            runtime
                .list_embedding_models(None)
                .await
                .expect("read-only registry query")
                .is_empty(),
            "configured models must remain in-memory only during read-only boot"
        );
    }

    #[test]
    fn explicit_readonly_constructor_uses_read_only_pool_even_on_writable_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("explicit_read_only_runtime.db");
        let config = RuntimeConfig {
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(path.clone()),
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
        KhiveRuntime::new(config.clone()).expect("create migrated database");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["-wal", "-shm"] {
                let mut name = path.file_name().expect("db file name").to_os_string();
                name.push(suffix);
                let sidecar = path.parent().expect("db parent dir").join(name);
                if sidecar.exists() {
                    let mut permissions = std::fs::metadata(&sidecar)
                        .expect("sidecar metadata")
                        .permissions();
                    permissions.set_mode(0o444);
                    std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
                }
            }
        }

        let runtime = KhiveRuntime::new_readonly(config).expect("explicit read-only boot");
        assert!(runtime.is_read_only());
        assert_eq!(
            runtime.backend().pool().writer_acquisition_snapshot(),
            khive_db::pool::WriterAcquisitionSnapshot::default(),
            "explicit read-only construction must validate through a reader without ever \
             acquiring the writer"
        );
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
                display_timezone: chrono_tz::Tz::UTC,
                events_split: None,
                db_path: Some(db_path),
                blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
    fn default_config_packs_loads_production_set() {
        let prior = std::env::var("KHIVE_PACKS").ok();
        // SAFETY: test function runs single-threaded; no other threads read or write KHIVE_PACKS.
        unsafe {
            std::env::remove_var("KHIVE_PACKS");
        }
        // The default distribution loads all production packs.
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.packs, RuntimeConfig::built_in_packs());
        assert!(cfg.packs.contains(&"kg".to_string()));
        assert!(cfg.packs.contains(&"gtd".to_string()));
        assert!(cfg.packs.contains(&"memory".to_string()));
        assert!(cfg.packs.contains(&"brain".to_string()));
        assert!(cfg.packs.contains(&"comm".to_string()));
        assert!(cfg.packs.contains(&"schedule".to_string()));
        assert!(cfg.packs.contains(&"knowledge".to_string()));
        // session loads by default so its background mirror warm-hook runs in
        // production; its handlers are all operator-only subhandlers (0 wire verbs).
        assert!(cfg.packs.contains(&"session".to_string()));
        assert!(cfg.packs.contains(&"git".to_string()));
        assert!(cfg.packs.contains(&"code".to_string()));
        assert!(cfg.packs.contains(&"workspace".to_string()));
        // blob loads by default; a normal file-backed boot installs a
        // default FsBlobStore beside the database file with no config
        // needed, so its verbs are live in default deployments too (only an
        // in-memory backend leaves them unconfigured).
        assert!(cfg.packs.contains(&"blob".to_string()));
        assert_eq!(cfg.packs.len(), 12);
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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

    // ---- [display] timezone (ADR-169) wiring tests ----

    #[test]
    fn runtime_config_from_khive_config_display_timezone_overrides_base() {
        let base = RuntimeConfig {
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display: crate::engine_config::DisplaySectionConfig {
                timezone: Some("America/New_York".to_string()),
            },
            ..KhiveConfig::default()
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.display_timezone,
            "America/New_York".parse::<chrono_tz::Tz>().unwrap(),
            "[display] timezone in khive.toml must override base.display_timezone"
        );
    }

    #[test]
    fn runtime_config_from_khive_config_absent_display_timezone_keeps_base() {
        let base = RuntimeConfig {
            git_write: Default::default(),
            display_timezone: "Asia/Tokyo".parse().unwrap(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
        let cfg = KhiveConfig::default(); // no [display] section
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.display_timezone,
            "Asia/Tokyo".parse::<chrono_tz::Tz>().unwrap(),
            "absent [display] timezone must preserve base.display_timezone unchanged"
        );
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: None,
            blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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
                display_timezone: chrono_tz::Tz::UTC,
                events_split: None,
                db_path: None,
                blob_hydration_bytes: crate::DEFAULT_BLOB_HYDRATION_BYTES,
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

    #[test]
    fn attachment_store_rejects_secondary_handle_and_accepts_its_core_projection() {
        let main_arc = migrated_memory_backend();
        let secondary_arc = migrated_memory_backend();
        let rt_secondary = KhiveRuntime::from_backend(secondary_arc, secondary_config())
            .with_core_backend(main_arc);

        let error = match rt_secondary.attachments() {
            Ok(_) => panic!("a secondary runtime must not expose attachment mutation"),
            Err(error) => error,
        };
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert!(
            error.to_string().contains("canonical main backend"),
            "secondary refusal must explain the liveness authority: {error}"
        );
        rt_secondary
            .core()
            .attachments()
            .expect("core projection must expose the main attachment store");
    }

    #[tokio::test]
    async fn record_plus_attachment_publication_rejects_a_secondary_runtime() {
        use khive_storage::{BlobStore as _, NewAttachment};

        let main_arc = migrated_memory_backend();
        let secondary_arc = migrated_memory_backend();
        let rt_secondary =
            KhiveRuntime::from_backend(Arc::clone(&secondary_arc), secondary_config())
                .with_core_backend(Arc::clone(&main_arc));
        let blob_root = tempfile::tempdir().expect("blob root");
        let blob_store = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
                .expect("blob store"),
        );
        let content_ref = blob_store.put(b"secondary-ref".to_vec()).await.unwrap();
        rt_secondary
            .install_blob_store(blob_store.clone())
            .expect("shared blob store");
        let token = rt_secondary.authorize(Namespace::local()).unwrap();

        let error = rt_secondary
            .create_entity_with_attachments(
                &token,
                "artifact",
                Some("visual_asset"),
                "must route through core",
                None,
                None,
                vec![],
                vec![NewAttachment {
                    role: "content".to_string(),
                    content_ref: content_ref.clone(),
                    media_type: None,
                    size_bytes: Some(13),
                }],
            )
            .await
            .expect_err("secondary attachment publication must fail closed");
        assert!(error.to_string().contains("canonical main backend"));
        assert!(rt_secondary
            .list_entities(&token, None, None, 10, 0)
            .await
            .unwrap()
            .is_empty());
        assert!(rt_secondary
            .core()
            .list_entities(&token, None, None, 10, 0)
            .await
            .unwrap()
            .is_empty());
        assert!(
            blob_store.exists(&content_ref).await.unwrap(),
            "refusal must not mutate the already-published object"
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

    #[test]
    fn named_vector_identity_rejects_ambiguous_or_unsafe_values() {
        assert!(NamedVectorIdentity::new("", "model", 4).is_err());
        assert!(NamedVectorIdentity::new("bad-key", "model", 4).is_err());
        assert!(NamedVectorIdentity::new("valid_key", " model", 4).is_err());
        assert!(NamedVectorIdentity::new("valid_key", "model", 0).is_err());
        assert!(NamedVectorIdentity::new("valid_key", "model", 8193).is_err());
        assert!(NamedVectorIdentity::new("k".repeat(128), "m".repeat(512), 4).is_ok());
        assert!(NamedVectorIdentity::new("k".repeat(129), "model", 4).is_err());
        assert!(NamedVectorIdentity::new("valid_key", "m".repeat(513), 4).is_err());
        assert_eq!(
            NamedVectorIdentity::new("valid_key", "model", 4)
                .expect("valid identity")
                .dimensions(),
            4
        );
    }

    #[tokio::test]
    async fn named_vector_store_rejects_dimension_or_model_key_rebinding() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let original = NamedVectorIdentity::new("visual_contract", "model-a", 4).unwrap();
        rt.vectors_for_named_identity(&token, &original)
            .await
            .expect("create named vector store");
        let registered = rt
            .list_embedding_models(Some("visual_contract"))
            .await
            .expect("list model registry");
        assert!(registered.iter().any(|record| {
            record.model_id == "model-a"
                && record.key_version == "visual_contract"
                && record.dimensions == 4
        }));
        let wrong_dimensions = NamedVectorIdentity::new("visual_contract", "model-a", 5).unwrap();
        let Err(dimension_error) = rt
            .vectors_for_named_identity(&token, &wrong_dimensions)
            .await
        else {
            panic!("same key cannot change dimensions");
        };
        assert!(dimension_error.to_string().contains("dimensions"));

        let wrong_model = NamedVectorIdentity::new("visual_contract", "model-b", 4).unwrap();
        let Err(model_error) = rt.vectors_for_named_identity(&token, &wrong_model).await else {
            panic!("same key cannot change model identity");
        };
        assert!(model_error.to_string().contains("already bound"));
    }

    #[tokio::test]
    async fn concurrent_named_vector_first_bind_has_one_immutable_winner() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let first = NamedVectorIdentity::new("visual_race", "model-a", 4).unwrap();
        let second = NamedVectorIdentity::new("visual_race", "model-b", 4).unwrap();

        let (first_result, second_result) = tokio::join!(
            rt.vectors_for_named_identity(&token, &first),
            rt.vectors_for_named_identity(&token, &second),
        );
        assert_ne!(
            first_result.is_ok(),
            second_result.is_ok(),
            "the active engine_name uniqueness rule must select exactly one first binding"
        );

        let (winner, loser) = if first_result.is_ok() {
            (&first, &second)
        } else {
            (&second, &first)
        };
        rt.vectors_for_named_identity(&token, winner)
            .await
            .expect("winning identity remains idempotent");
        let error = match rt.vectors_for_named_identity(&token, loser).await {
            Ok(_) => panic!("losing identity cannot rebind the empty table"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("already bound"));

        let registered = rt
            .list_embedding_models(Some("visual_race"))
            .await
            .expect("list race registry");
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].model_id, winner.model_name());
    }

    #[tokio::test]
    async fn named_vector_registry_keeps_immutable_revisions_active_together() {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let first = NamedVectorIdentity::new("visual_revision_a", "visual-model", 4).unwrap();
        let second = NamedVectorIdentity::new("visual_revision_b", "visual-model", 4).unwrap();

        rt.vectors_for_named_identity(&token, &first)
            .await
            .expect("open first immutable space");
        rt.vectors_for_named_identity(&token, &second)
            .await
            .expect("open second immutable space");

        let registered = rt.list_embedding_models(None).await.expect("list registry");
        assert!(registered.iter().any(|record| {
            record.engine_name == "visual_revision_a"
                && record.model_id == "visual-model"
                && record.key_version == "visual_revision_a"
                && record.status == "active"
        }));
        assert!(registered.iter().any(|record| {
            record.engine_name == "visual_revision_b"
                && record.model_id == "visual-model"
                && record.key_version == "visual_revision_b"
                && record.status == "active"
        }));
    }
}
