//! KhiveRuntime — composable handle to all storage capabilities.
//!
//! `RuntimeConfig`, `BackendId`, `NamespaceToken`, and embedding model helpers
//! live in `super::config` and are re-exported from here.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use khive_db::{ConnectionPool, StorageBackend};
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
use crate::note_search_ann::NoteSearchAnnProvider;
use crate::pack::KindHook;

#[path = "runtime/config_access.rs"]
mod config_access;
mod embedder_init;
mod events_disk_policy;
mod serving_policy;

#[cfg(all(test, target_os = "macos"))]
const IN_PROCESS_TEST_NOFILE_LIMIT: libc::rlim_t = 4096;
#[cfg(all(test, target_os = "macos"))]
static IN_PROCESS_TEST_NOFILE_INIT: std::sync::Once = std::sync::Once::new();

#[cfg(all(test, target_os = "macos"))]
fn ensure_in_process_test_nofile_limit() {
    IN_PROCESS_TEST_NOFILE_INIT.call_once(|| {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limits` is writable, and only this test binary's soft
        // limit may change; the inherited hard limit is preserved.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) }, 0);
        assert!(
            limits.rlim_max >= IN_PROCESS_TEST_NOFILE_LIMIT,
            "in-process SQLite tests require a hard open-file limit of at least {IN_PROCESS_TEST_NOFILE_LIMIT}"
        );
        if limits.rlim_cur < IN_PROCESS_TEST_NOFILE_LIMIT {
            limits.rlim_cur = IN_PROCESS_TEST_NOFILE_LIMIT;
            // SAFETY: the new soft limit does not exceed the observed hard
            // limit, which is left unchanged.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) }, 0);
        }
    });
}

tokio::task_local! {
    static REQUEST_EMBEDDER_EXCLUSIONS: Arc<HashSet<String>>;
}

/// Run one request with daemon-only embedding models excluded from registry access.
pub fn scope_request_embedder_exclusions<F>(
    excluded: Vec<String>,
    future: F,
) -> impl Future<Output = F::Output>
where
    F: Future,
{
    REQUEST_EMBEDDER_EXCLUSIONS.scope(Arc::new(excluded.into_iter().collect()), future)
}

/// Carry the current request's embedder exclusions into a spawned task.
pub fn inherit_request_embedder_scope<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    let exclusions = REQUEST_EMBEDDER_EXCLUSIONS.try_with(Arc::clone).ok();
    async move {
        match exclusions {
            Some(exclusions) => REQUEST_EMBEDDER_EXCLUSIONS.scope(exclusions, future).await,
            None => future.await,
        }
    }
}

fn request_excludes_embedder(name: &str) -> bool {
    let canonical = parse_embedding_model_alias(name)
        .map(|model| model.to_string())
        .unwrap_or_else(|| name.to_string());
    REQUEST_EMBEDDER_EXCLUSIONS
        .try_with(|excluded| excluded.contains(&canonical))
        .unwrap_or(false)
}

/// Callback type for pack-installed entity-type validators.
///
/// Receives `(kind, entity_type)` and returns the normalised type string,
/// or `RuntimeError::InvalidInput` if the type is not registered for that kind.
/// When `entity_type` is `None`, the implementation must return `Ok(None)`.
pub type EntityTypeValidatorFn =
    Arc<dyn Fn(&str, Option<&str>) -> Result<Option<String>, RuntimeError> + Send + Sync>;

/// Pack-aggregated entity-kind update hooks: `(entity kind, hook)` for every
/// kind whose owning pack both declares it and registers a `KindHook`.
///
/// Named rather than written inline because the runtime stores it behind an
/// `Arc<RwLock<..>>` and passes it across the transport boundary, so the bare
/// form appears three times and reads as noise at each one.
pub type EntityKindHooks = Vec<(String, Arc<dyn KindHook>)>;

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

struct CachedNamedVectorStores {
    identity: NamedVectorIdentity,
    by_namespace: HashMap<String, Arc<dyn VectorStore>>,
}

fn check_cached_named_vector_identity(
    cached: &NamedVectorIdentity,
    requested: &NamedVectorIdentity,
) -> RuntimeResult<()> {
    if cached.dimensions() != requested.dimensions() {
        return Err(RuntimeError::InvalidInput(format!(
            "named vector model_key {:?} is already bound to {} dimensions, expected {}",
            requested.model_key(),
            cached.dimensions(),
            requested.dimensions()
        )));
    }
    if cached.model_name() != requested.model_name() {
        return Err(RuntimeError::InvalidInput(format!(
            "named vector model_key {:?} is already bound to a different active model identity",
            requested.model_key()
        )));
    }
    Ok(())
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
    BackendId, BackendIdError, NamespaceToken, RuntimeConfig,
};

// ---- KhiveRuntime ----

/// Composable runtime handle used by the MCP server.
///
/// Wraps a `StorageBackend` and provides namespace-scoped accessor methods
/// Snapshot of the main runtime's embedder wiring (registry handle, default
/// name, and the config model fields the default-resolution path reads).
/// Carried by secondary-pack runtimes; consumed by [`KhiveRuntime::core`].
#[derive(Clone)]
struct CoreEmbedderState {
    registry: Arc<std::sync::RwLock<crate::embedder_registry::EmbedderRegistry>>,
    default_embedder_name: Arc<str>,
    embedding_model: Option<EmbeddingModel>,
    additional_embedding_models: Vec<EmbeddingModel>,
}

#[derive(Clone)]
struct NoteKindEntry {
    name: String,
    embedding_policy: crate::NoteEmbeddingPolicy,
    registered: bool,
}

/// An already-open serving backend eligible for operator diagnostics.
/// Aliases of the same canonical database file share one entry.
#[derive(Clone)]
pub struct OpenedDiagnosticBackend {
    pub backend_names: Vec<String>,
    pub canonical_path: Option<PathBuf>,
    pub pool: Arc<ConnectionPool>,
}

struct LateOpenedDiagnosticBackend {
    backend_names: Vec<&'static str>,
    pool: Weak<ConnectionPool>,
}

fn same_diagnostic_database(a: &OpenedDiagnosticBackend, b: &OpenedDiagnosticBackend) -> bool {
    match (a.pool.canonical_path(), b.pool.canonical_path()) {
        (Some(a), Some(b)) => a == b,
        (None, None) => Arc::ptr_eq(&a.pool, &b.pool),
        _ => false,
    }
}

/// for each storage capability, plus a lazily-loaded embedder.
#[derive(Clone)]
pub struct KhiveRuntime {
    pub(crate) visibility_receipts: Arc<crate::visibility_receipts::ReceiptCapability>,
    pub(crate) visibility_cutover: Arc<crate::visibility_receipts::ReceiptCutover>,
    core_visibility_cutover: Arc<crate::visibility_receipts::ReceiptCutover>,
    backend: Arc<StorageBackend>,
    /// Successful named-vector bindings and their namespace-scoped stores.
    /// Shared by runtime clones so repeated reads do not enter the writer or
    /// rescan the vector table after the first verified binding.
    named_vector_stores: Arc<RwLock<HashMap<String, CachedNamedVectorStores>>>,
    /// The main backend's cache, used when a secondary runtime creates a
    /// `core()` handle. It must never reuse a secondary backend's store.
    core_named_vector_stores: Option<Arc<RwLock<HashMap<String, CachedNamedVectorStores>>>>,
    /// When `Some`, holds the main backend so that `core()` can return a
    /// main-bound runtime handle without constructing a new connection.
    /// `None` when this runtime is already bound to the main backend.
    core_backend: Option<Arc<StorageBackend>>,
    config: RuntimeConfig,
    outbound_email_policy: crate::OutboundEmailPolicy,
    /// All SQLite backends declared by the host process, including those
    /// assigned to other packs. The code pack fences these from ingest.
    declared_backend_db_paths: Arc<[PathBuf]>,
    /// Pools opened during serving host composition, grouped by canonical
    /// database file.
    diagnostic_backends: Arc<[OpenedDiagnosticBackend]>,
    /// Pools opened later by this serving runtime and its pack handles.
    /// Weak references keep diagnostics from extending their lifetime.
    late_diagnostic_backends: Arc<Mutex<Vec<LateOpenedDiagnosticBackend>>>,
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
    /// The MAIN runtime's embedder wiring, carried by secondary-backend
    /// runtimes so that `core()`-routed writes embed with the main runtime's
    /// models even when this pack's own registry is empty (`no_embed`).
    /// `None` on the main runtime, and on secondaries wired before the boot
    /// path calls [`with_core_embedders_from`](Self::with_core_embedders_from)
    /// — `core()` then falls back to this runtime's own embedder state.
    core_embedders: Option<CoreEmbedderState>,
    /// Pack-extensible edge endpoint rules. Shared across clones
    /// via `Arc<RwLock<_>>`; installed once by the transport after the
    /// `VerbRegistry` is built. Empty until installed
    edge_rules: Arc<RwLock<Vec<EdgeEndpointRule>>>,
    /// Pack-aggregated valid entity kinds and note-kind policy entries.
    ///
    /// Installed by the transport layer after building the `VerbRegistry`.
    /// When non-empty, `create_entity`, `create_note_inner`, and `import_kg`
    /// reject kinds not in these sets. When empty (no packs loaded, e.g.
    /// bare runtime in unit tests), kind validation is skipped — the pack
    /// handler layer is the primary enforcement point.
    valid_entity_kinds: Arc<RwLock<Vec<String>>>,
    valid_note_kinds: Arc<RwLock<Vec<NoteKindEntry>>>,
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
    /// Backend-matched, pack-installed ANN source for note search. Absent on a
    /// bare runtime or when no memory graph provider serves this backend.
    note_search_ann_provider: Arc<RwLock<Option<Arc<dyn NoteSearchAnnProvider>>>>,
    /// Pack-installed note-write validator.
    ///
    /// When `Some`, every runtime note-materialisation site that accepts
    /// caller-supplied `properties` routes them through this function before
    /// the `Note` is built, so a pack-owned identity property is derived from
    /// the authorization token instead of trusted from caller input. `None`
    /// on a bare runtime (no packs) — the properties pass through unchanged.
    note_write_validator: Arc<RwLock<Option<NoteWriteValidatorFn>>>,
    /// Pack-installed entity-kind update-validation hooks (issue #2943).
    ///
    /// Every `(entity kind, hook)` pair for which an owning pack declares
    /// the entity kind and registers a `KindHook`, aggregated once by
    /// `VerbRegistry::entity_kind_hooks` and installed by the transport
    /// after the registry is built — same timing and rationale as
    /// `entity_type_validator`: `khive-runtime` does not hold a
    /// `VerbRegistry`, so this is the extension point that lets
    /// `prepare_guarded_entity_update` reach a pack's `KindHook` on the
    /// generic entity `update` path, the counterpart to
    /// `prepare_note_update_hook` on the note side. Empty until installed
    /// (bare runtime, or no pack registers an entity-kind hook), which
    /// leaves the dispatch a no-op.
    entity_kind_hooks: Arc<RwLock<EntityKindHooks>>,
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
    pub fn new(mut config: RuntimeConfig) -> RuntimeResult<Self> {
        let wal_ceiling = config.resolve_wal_ceiling_policy(false)?;
        let disk_guard = config.resolve_disk_guard_policy(false)?;
        // Refuse a missing lock directory before the constructor below creates
        // the database's parent directory.
        let volume_lock_dir = if config.db_path.is_some() {
            let configured = config.volume_lock_dir.clone();
            Some(khive_db::require_volume_lock_dir(configured)?)
        } else {
            None
        };
        Self::new_with_file_backend(config, true, |path| {
            StorageBackend::sqlite_with_max_readers_and_policies(
                path,
                None,
                wal_ceiling,
                disk_guard.expect("file-backed disk policy"),
                volume_lock_dir.expect("file-backed volume-lock directory"),
            )
        })
    }

    /// Construct a fixture runtime with a small concurrent reader pool.
    #[cfg(any(test, feature = "test-internals"))]
    pub fn new_for_test(mut config: RuntimeConfig) -> RuntimeResult<Self> {
        let wal_ceiling = config.resolve_wal_ceiling_policy(false)?;
        let disk_guard = config.resolve_disk_guard_policy(false)?;
        Self::new_with_file_backend(config, true, |path| {
            StorageBackend::sqlite_for_test_with_policies(
                path,
                wal_ceiling,
                disk_guard.expect("file-backed disk policy"),
            )
        })
    }

    pub(crate) fn new_with_file_backend(
        config: RuntimeConfig,
        create_parent: bool,
        open_file: impl FnOnce(&std::path::Path) -> Result<StorageBackend, khive_db::SqliteError>,
    ) -> RuntimeResult<Self> {
        #[cfg(unix)]
        crate::events_split::socket_path::validate_configured_events_socket(&config)?;
        #[cfg(all(test, target_os = "macos"))]
        ensure_in_process_test_nofile_limit();
        let backend = match &config.db_path {
            Some(path) => {
                if let Some(parent) = path.parent().filter(|_| create_parent) {
                    std::fs::create_dir_all(parent).ok();
                }
                open_file(path)?
            }
            None => {
                if config.wal_ceiling_configured_bytes != 0 || config.wal_ceiling_bytes != 0 {
                    return Err(khive_db::SqliteError::InvalidConfig(
                        "nonzero wal_ceiling_bytes requires a file-backed SQLite backend".into(),
                    )
                    .into());
                }
                StorageBackend::memory()?
            }
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
        Ok(Self::assemble_from_backend(
            Arc::new(backend),
            config,
            false,
        ))
    }

    /// Open a runtime for read-only inspection (no model registration, no DB creation).
    ///
    /// File-backed databases are opened with SQLite read-only/query-only flags
    /// and must already be at this build's current schema version. No migrations
    /// or configured-model registration writes are attempted. A `None` path
    /// retains the historical ephemeral in-memory behavior for tests.
    pub fn new_readonly(mut config: RuntimeConfig) -> RuntimeResult<Self> {
        let wal_ceiling = config.resolve_wal_ceiling_policy(true)?;
        Self::new_readonly_with_file_backend(config, |path| {
            StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
                path,
                None,
                wal_ceiling,
            )
        })
    }

    /// Construct a read-only fixture runtime with a small reader pool.
    #[cfg(any(test, feature = "test-internals"))]
    pub fn new_readonly_for_test(mut config: RuntimeConfig) -> RuntimeResult<Self> {
        let wal_ceiling = config.resolve_wal_ceiling_policy(true)?;
        Self::new_readonly_with_file_backend(config, |path| {
            StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
                path,
                Some(2),
                wal_ceiling,
            )
        })
    }

    fn new_readonly_with_file_backend(
        config: RuntimeConfig,
        open_file: impl FnOnce(&std::path::Path) -> Result<StorageBackend, khive_db::SqliteError>,
    ) -> RuntimeResult<Self> {
        #[cfg(unix)]
        crate::events_split::socket_path::validate_configured_events_socket(&config)?;
        #[cfg(all(test, target_os = "macos"))]
        ensure_in_process_test_nofile_limit();
        let backend = match &config.db_path {
            Some(path) => open_file(path)?,
            None => {
                if config.wal_ceiling_configured_bytes != 0 || config.wal_ceiling_bytes != 0 {
                    return Err(khive_db::SqliteError::InvalidConfig(
                        "nonzero wal_ceiling_bytes requires a file-backed SQLite backend".into(),
                    )
                    .into());
                }
                StorageBackend::memory()?
            }
        };
        backend.prepare_core_schema()?;
        Ok(Self::assemble_from_backend(
            Arc::new(backend),
            config,
            false,
        ))
    }

    /// Construct a runtime from an already-opened backend.
    ///
    /// This is a low-level, infallible assembly seam for already-prepared
    /// multi-backend deployments. It does not migrate or require completion of
    /// the V21 attachment cutover, and it does not read the backend: receipt
    /// admission is checked by the first receipt operation and cached once it
    /// succeeds. Production hosts must first run the async kkernel/khive-mcp
    /// coordinator and must not expose a server over a pending or incomplete
    /// backend. Prefer [`Self::from_prepared_backend`] when constructing one
    /// fallible host runtime.
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
        Self::assemble_from_backend(backend, config, false)
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
        backend.validate_memory_visibility_cutover()?;
        if !backend.is_read_only() {
            register_configured_embedding_models(&backend, &config)?;
        }
        Ok(Self::assemble_from_backend(backend, config, true))
    }

    fn assemble_from_backend(
        backend: Arc<StorageBackend>,
        config: RuntimeConfig,
        cutover_validated: bool,
    ) -> Self {
        if config.backend_id.as_str() == BackendId::MAIN {
            backend.pool().main_pool_generation();
        }
        let ann_fresh_tail_enabled = crate::config::ann_fresh_tail_enabled_from_env();
        let (registry, default_embedder_name) = build_embedder_registry(&config);
        let visibility_receipts = Arc::new(
            crate::visibility_receipts::ReceiptCapability::from_config(&config),
        );
        let visibility_cutover = Arc::new(crate::visibility_receipts::ReceiptCutover::new(
            backend.clone(),
            cutover_validated,
        ));
        Self {
            visibility_receipts,
            core_visibility_cutover: visibility_cutover.clone(),
            visibility_cutover,
            backend,
            named_vector_stores: Arc::new(RwLock::new(HashMap::new())),
            core_named_vector_stores: None,
            core_backend: None,
            config,
            outbound_email_policy: Default::default(),
            declared_backend_db_paths: Vec::new().into(),
            diagnostic_backends: Vec::new().into(),
            late_diagnostic_backends: Arc::new(Mutex::new(Vec::new())),
            ann_fresh_tail_enabled,
            embedder_registry: Arc::new(std::sync::RwLock::new(registry)),
            default_embedder_name,
            core_embedders: None,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
            valid_entity_kinds: Arc::new(RwLock::new(Vec::new())),
            valid_note_kinds: Arc::new(RwLock::new(Vec::new())),
            entity_type_validator: Arc::new(RwLock::new(None)),
            note_mutation_hook: Arc::new(RwLock::new(None)),
            note_search_ann_provider: Arc::new(RwLock::new(None)),
            note_write_validator: Arc::new(RwLock::new(None)),
            entity_kind_hooks: Arc::new(RwLock::new(Vec::new())),
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
    /// Binding a different core clears its named-vector cache and prior main
    /// embedder wiring. Call [`Self::with_core_embedders_from`] with the new main
    /// runtime after rebinding when core-routed writes require its embedders.
    ///
    /// Panics in debug builds if `self.config.backend_id == BackendId::MAIN`,
    /// because the main runtime does not need a core pointer.
    pub fn with_core_backend(mut self, core: Arc<StorageBackend>) -> Self {
        debug_assert_ne!(
            self.config.backend_id.as_str(),
            BackendId::MAIN,
            "with_core_backend must not be called on the main runtime"
        );
        core.pool().main_pool_generation();
        if self.visibility_cutover.is_bound_to(&core) {
            self.core_visibility_cutover = self.visibility_cutover.clone();
        } else if !self.core_visibility_cutover.is_bound_to(&core) {
            self.core_visibility_cutover = Arc::new(
                crate::visibility_receipts::ReceiptCutover::new(core.clone(), false),
            );
        }
        if self
            .core_backend
            .as_ref()
            .is_some_and(|previous| !Arc::ptr_eq(previous, &core))
        {
            self.core_named_vector_stores = None;
            self.core_embedders = None;
        }
        if self.core_named_vector_stores.is_none() {
            self.core_named_vector_stores = Some(Arc::new(RwLock::new(HashMap::new())));
        }
        self.core_backend = Some(core);
        self
    }

    /// Carry the main runtime's embedder wiring for `core()`-routed writes.
    ///
    /// Boot-path companion to [`with_core_backend`](Self::with_core_backend).
    /// Without it, `core()` shares this pack runtime's own embedder registry —
    /// which under `[packs.<name>] no_embed = true` is empty, so core-routed
    /// concept writes would silently skip embedding on the shared graph.
    pub fn with_core_embedders_from(mut self, main: &KhiveRuntime) -> Self {
        debug_assert!(
            main.core_backend.is_none(),
            "with_core_embedders_from takes the MAIN runtime"
        );
        self.core_embedders = Some(CoreEmbedderState {
            registry: main.embedder_registry.clone(),
            default_embedder_name: main.default_embedder_name.clone(),
            embedding_model: main.config.embedding_model,
            additional_embedding_models: main.config.additional_embedding_models.clone(),
        });
        self.core_named_vector_stores = Some(main.named_vector_stores.clone());
        if Arc::ptr_eq(&self.backend, &main.backend) {
            self.named_vector_stores = main.named_vector_stores.clone();
        }
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
    /// `entity_type_validator`, `note_mutation_hook`, `entity_kind_hooks`) with `self`.
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
            // A main-assigned pack runtime has no core pointer, but may still
            // carry main's embedder wiring: with `no_embed` its OWN registry
            // is empty, and core-routed concept writes must embed regardless
            // of which backend the pack was assigned to.
            None => match &self.core_embedders {
                None => self.clone(),
                Some(core_embedders) => {
                    let mut core = self.clone();
                    core.config.embedding_model = core_embedders.embedding_model;
                    core.config.additional_embedding_models =
                        core_embedders.additional_embedding_models.clone();
                    core.embedder_registry = core_embedders.registry.clone();
                    core.default_embedder_name = core_embedders.default_embedder_name.clone();
                    core.core_embedders = None;
                    core
                }
            },
            Some(main_arc) => {
                let mut core_config = self.config.clone();
                core_config.backend_id = BackendId::main();
                // Core-routed writes embed with the MAIN runtime's wiring when
                // the boot path supplied it (see `with_core_embedders_from`);
                // both the registry handle and the config model fields must
                // come from main, since default-model resolution reads
                // `config.embedding_model` (`resolve_embedding_model`).
                let (embedder_registry, default_embedder_name) = match &self.core_embedders {
                    Some(core_embedders) => {
                        core_config.embedding_model = core_embedders.embedding_model;
                        core_config.additional_embedding_models =
                            core_embedders.additional_embedding_models.clone();
                        (
                            core_embedders.registry.clone(),
                            core_embedders.default_embedder_name.clone(),
                        )
                    }
                    None => (
                        self.embedder_registry.clone(),
                        self.default_embedder_name.clone(),
                    ),
                };
                KhiveRuntime {
                    visibility_receipts: self.visibility_receipts.clone(),
                    visibility_cutover: self.core_visibility_cutover.clone(),
                    core_visibility_cutover: self.core_visibility_cutover.clone(),
                    backend: main_arc.clone(),
                    named_vector_stores: self
                        .core_named_vector_stores
                        .clone()
                        .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
                    core_named_vector_stores: None,
                    core_backend: None,
                    config: core_config,
                    outbound_email_policy: self.outbound_email_policy.clone(),
                    declared_backend_db_paths: self.declared_backend_db_paths.clone(),
                    diagnostic_backends: self.diagnostic_backends.clone(),
                    late_diagnostic_backends: self.late_diagnostic_backends.clone(),
                    ann_fresh_tail_enabled: self.ann_fresh_tail_enabled,
                    embedder_registry,
                    default_embedder_name,
                    core_embedders: None,
                    edge_rules: self.edge_rules.clone(),
                    valid_entity_kinds: self.valid_entity_kinds.clone(),
                    valid_note_kinds: self.valid_note_kinds.clone(),
                    entity_type_validator: self.entity_type_validator.clone(),
                    note_mutation_hook: self.note_mutation_hook.clone(),
                    note_search_ann_provider: self.note_search_ann_provider.clone(),
                    note_write_validator: self.note_write_validator.clone(),
                    entity_kind_hooks: self.entity_kind_hooks.clone(),
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

    /// Whether two runtime handles share one opened physical store. The host
    /// deduplicates same-path aliases; separately opened hard-link aliases
    /// compare the identity pinned when SQLite opened each file.
    pub fn shares_backend_storage_with(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.backend, &other.backend) {
            return true;
        }
        #[cfg(any(unix, windows))]
        {
            matches!(
                (
                    self.backend.pool().opened_file_identity_record(),
                    other.backend.pool().opened_file_identity_record()
                ),
                (Some(left), Some(right)) if left == right
            )
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    /// Install only pools the host actually opened. A bare runtime defaults
    /// to its already-open main pool without opening or creating another file.
    pub fn with_diagnostic_backends(mut self, backends: Arc<[OpenedDiagnosticBackend]>) -> Self {
        self.diagnostic_backends = backends;
        self
    }

    /// Share late-opened diagnostics with pack runtimes from the same serving
    /// host. Independent runtimes retain independent observers.
    pub fn with_diagnostic_observer_from(mut self, main: &KhiveRuntime) -> Self {
        self.late_diagnostic_backends = Arc::clone(&main.late_diagnostic_backends);
        self
    }

    fn register_late_diagnostic_pool(
        &self,
        backend_name: &'static str,
        pool: &Arc<ConnectionPool>,
    ) {
        let mut backends = self
            .late_diagnostic_backends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        backends.retain(|entry| entry.pool.strong_count() > 0);
        if let Some(existing) = backends.iter_mut().find(|entry| {
            entry
                .pool
                .upgrade()
                .is_some_and(|live| Arc::ptr_eq(&live, pool))
        }) {
            if !existing.backend_names.contains(&backend_name) {
                existing.backend_names.push(backend_name);
            }
            return;
        }
        backends.push(LateOpenedDiagnosticBackend {
            backend_names: vec![backend_name],
            pool: Arc::downgrade(pool),
        });
    }

    fn live_late_diagnostic_backends(&self) -> Vec<OpenedDiagnosticBackend> {
        let mut backends = self
            .late_diagnostic_backends
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut live = Vec::new();
        backends.retain(|entry| {
            let Some(pool) = entry.pool.upgrade() else {
                return false;
            };
            live.push(OpenedDiagnosticBackend {
                backend_names: entry
                    .backend_names
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
                canonical_path: pool.canonical_path().map(PathBuf::from),
                pool,
            });
            true
        });
        live
    }

    pub fn diagnostic_backends(&self) -> Arc<[OpenedDiagnosticBackend]> {
        let mut opened: Vec<OpenedDiagnosticBackend> = if self.diagnostic_backends.is_empty() {
            let main = self.core().backend.pool_arc();
            vec![OpenedDiagnosticBackend {
                backend_names: vec![BackendId::MAIN.to_string()],
                canonical_path: main.canonical_path().map(PathBuf::from),
                pool: main,
            }]
        } else {
            self.diagnostic_backends.iter().cloned().collect()
        };
        for late in self.live_late_diagnostic_backends() {
            if let Some(existing) = opened
                .iter_mut()
                .find(|existing| same_diagnostic_database(existing, &late))
            {
                for name in late.backend_names {
                    if !existing.backend_names.contains(&name) {
                        existing.backend_names.push(name);
                    }
                }
            } else {
                opened.push(late);
            }
        }
        opened.into()
    }

    /// Whether this runtime selects the vector arm for a hybrid search —
    /// true exactly when a default embedding model is configured. Single
    /// source of truth for the policy every fan-out and single-backend
    /// dispatch path uses to report `arm_participation`/`vector_selected`.
    pub fn vector_arm_selected(&self) -> bool {
        self.config.embedding_model.is_some()
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
        // Match housekeeping's compiled legacy-record fallback (ADR-091
        // Amendment 6), independent of checkpoint or local sweep overrides.
        let legacy_sweep_interval = khive_db::SessionSweepConfig::default().interval;
        let build_hash = crate::build_info::BUILD_INFO
            .is_stamped()
            .then_some(crate::build_info::BUILD_INFO.source_revision);
        let build = khive_db::diagnostics::BuildIdentity::from_env(
            crate::build_info::PACKAGE_VERSION,
            build_hash,
        );

        let mut report = khive_db::diagnostics::collect_with_runtime_audit_metrics_interruptibly(
            pool,
            build,
            legacy_sweep_interval,
            crate::pack::audit_append_failure_count(),
            runtime_audit_batch_metrics,
        )
        .await
        .map_err(RuntimeError::from)?;
        report.writer_contention.audit_obligation_append_failures =
            Some(crate::pack::audit_obligation_append_failure_count());
        report
            .writer_contention
            .audit_obligation_append_failures_unavailable_reason = None;
        let (ann_routes, fallback_routes) = crate::note_search_ann::route_totals();
        report.note_search_ann_route_total = ann_routes;
        report.note_search_fallback_route_total = fallback_routes;
        Ok(report)
    }

    /// Collect the same per-file report as the primary diagnostic surface for
    /// one opened backend. The process identity comes from main; invoking
    /// `ProcessIdentity::current` on a secondary would miscount main generations.
    pub async fn db_diagnostics_for_opened_backend_with_audit_metrics(
        &self,
        backend: &OpenedDiagnosticBackend,
        runtime_audit_batch_metrics: Option<khive_db::diagnostics::RuntimeAuditBatchMetrics>,
    ) -> RuntimeResult<khive_db::diagnostics::DbDiagnostics> {
        let main_pool = self.core().backend.pool_arc();
        let process = khive_db::diagnostics::ProcessIdentity::current(&main_pool);
        let build_hash = crate::build_info::BUILD_INFO
            .is_stamped()
            .then_some(crate::build_info::BUILD_INFO.source_revision);
        let build = khive_db::diagnostics::BuildIdentity::from_env(
            crate::build_info::PACKAGE_VERSION,
            build_hash,
        );
        let mut report =
            khive_db::diagnostics::collect_with_runtime_audit_metrics_for_process_interruptibly(
                Arc::clone(&backend.pool),
                build,
                process,
                khive_db::SessionSweepConfig::default().interval,
                crate::pack::audit_append_failure_count(),
                runtime_audit_batch_metrics,
            )
            .await
            .map_err(RuntimeError::from)?;
        report.writer_contention.audit_obligation_append_failures =
            Some(crate::pack::audit_obligation_append_failure_count());
        report
            .writer_contention
            .audit_obligation_append_failures_unavailable_reason = None;
        let (ann_routes, fallback_routes) = crate::note_search_ann::route_totals();
        report.note_search_ann_route_total = ann_routes;
        report.note_search_fallback_route_total = fallback_routes;
        Ok(report)
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
    /// patching the evidence onto it afterward. Full-row writes also preserve
    /// existing channel-health coordinates while allowing heartbeat metadata to
    /// change. The trusted channel-ingest path does not go through this accessor; see
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
    /// measured bulk of event write volume — persists to the events database
    /// (forwarded over the events daemon socket in daemon deployments, or
    /// opened directly in embedded/one-shot contexts), while plain appends
    /// stay on this runtime's backend, keeping every raw-SQL consumer of the
    /// legacy `events` table (schedule provenance, kg projection guards,
    /// GraphQuery's substrate union) correct by construction. Reads merge
    /// both stores. Unconfigured runtimes (tests, in-memory) keep the legacy
    /// main-store behavior. Every returned store is decorated at this typed
    /// accessor boundary so append callers cannot override the namespace or
    /// actor resolved into the sealed authorization token.
    pub fn events(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn EventStore>> {
        Ok(crate::event_store_guard::AttributedEventStore::wrap(
            self.raw_events_for_namespace(token.namespace().as_str())?,
            token,
        ))
    }

    /// The event sidecar inherits the already-open MAIN pool's WAL policy.
    /// A secondary pack's config or pool does not govern this shared file.
    fn events_wal_ceiling_policy(&self) -> khive_db::WalCeilingPolicy {
        self.core_backend
            .as_ref()
            .unwrap_or(&self.backend)
            .pool()
            .config()
            .wal_ceiling
    }

    /// Build the undecorated event store used only by the registry's trusted
    /// audit composer, which stamps from each resolved `GateRequest` before
    /// enqueueing. Pack/runtime call sites must use [`Self::events`] instead.
    pub(crate) fn raw_events_for_namespace(
        &self,
        namespace: &str,
    ) -> RuntimeResult<Arc<dyn EventStore>> {
        let legacy = self.backend.events_for_namespace(namespace)?;
        match &self.config.events_split {
            None => Ok(legacy),
            Some(split) => {
                // Read-only is decided before the transport question: a
                // read-only runtime must neither create nor schema-initialize
                // an events database, and it must not forward writes to the
                // events daemon either — a socket in the config describes the
                // deployment, not this process's authority. Serve merged
                // reads from a read-only open of the sidecar when it exists
                // (the storage-layer read-only binding refuses any write that
                // slips through), and the legacy store alone otherwise: no
                // sidecar on disk means no lane rows exist, so minting the
                // file just to read nothing from it would be a write in
                // disguise.
                if self.backend.is_read_only() {
                    if !split.db_path.exists() {
                        return Ok(legacy);
                    }
                    let lane_backend = crate::events_split::direct_backend_with_policies(
                        &split.db_path,
                        true,
                        Some(self.backend.pool().config().max_readers),
                        self.events_wal_ceiling_policy(),
                        None,
                        self.events_volume_lock_dir(),
                    )?;
                    self.register_late_diagnostic_pool("events", &lane_backend.pool_arc());
                    let lane = lane_backend.events_for_namespace(namespace)?;
                    return Ok(Arc::new(crate::events_split::SplitEventStore::new(
                        legacy, lane,
                    )));
                }
                let lane: Arc<dyn EventStore> = match &split.socket_path {
                    #[cfg(unix)]
                    Some(socket) => {
                        let client = crate::events_split::client_for(socket)?;
                        Arc::new(crate::events_split::ForwardingEventStore::new(
                            namespace, client,
                        ))
                    }
                    #[cfg(not(unix))]
                    Some(_socket) => {
                        return Err(RuntimeError::InvalidInput(
                            "events-daemon socket forwarding requires a Unix platform; \
                             configure the events split in direct mode here"
                                .to_string(),
                        ));
                    }
                    None => {
                        let lane_backend = crate::events_split::direct_backend_with_policies(
                            &split.db_path,
                            false,
                            Some(self.backend.pool().config().max_readers),
                            self.events_wal_ceiling_policy(),
                            Some(self.events_disk_guard_policy()?),
                            self.events_volume_lock_dir(),
                        )?;
                        self.register_late_diagnostic_pool("events", &lane_backend.pool_arc());
                        lane_backend.events_for_namespace(namespace)?
                    }
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

    /// SQL access to the events-split sidecar database for read purposes,
    /// when the split (ADR-170) is configured and the sidecar exists on
    /// disk. `None` means every event row lives in the legacy `events`
    /// table, so a raw-SQL consumer needs no second lookup. Consumers that
    /// resolve an event by id or hex prefix against `self.sql()` must also
    /// consult this store on a miss: the audit-batch lane's rows live only
    /// in the sidecar.
    ///
    /// A writable runtime opens the sidecar through the ordinary writable
    /// binding: WAL supports one-writer-many-readers, and the read-only
    /// binding's frozen-snapshot guard refuses any sidecar with a live
    /// writer's `-shm` beside it — exactly the live-deployment case these
    /// reads exist for. A read-only runtime keeps the read-only open (it
    /// must neither create nor schema-initialize a sidecar), which serves
    /// genuinely frozen snapshots and refuses live ones, matching the
    /// read-only arm of `events()`. Never creates a sidecar as a side
    /// effect of a read.
    pub fn events_sidecar_sql_read_only(&self) -> RuntimeResult<Option<Arc<dyn SqlAccess>>> {
        match &self.config.events_split {
            None => Ok(None),
            Some(split) => {
                if !split.db_path.exists() {
                    return Ok(None);
                }
                let backend = if self.backend.is_read_only() {
                    crate::events_split::direct_backend_with_policies(
                        &split.db_path,
                        true,
                        Some(self.backend.pool().config().max_readers),
                        self.events_wal_ceiling_policy(),
                        None,
                        self.events_volume_lock_dir(),
                    )?
                } else {
                    crate::events_split::direct_backend_with_policies(
                        &split.db_path,
                        false,
                        Some(self.backend.pool().config().max_readers),
                        self.events_wal_ceiling_policy(),
                        Some(self.events_disk_guard_policy()?),
                        self.events_volume_lock_dir(),
                    )?
                };
                self.register_late_diagnostic_pool("events", &backend.pool_arc());
                Ok(Some(backend.sql()))
            }
        }
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
        let (model_name, dims) = self.vector_model_metadata(model_name)?;
        Ok(self.backend.vectors_for_namespace(
            &sanitize_key(&model_name),
            &model_name,
            dims,
            token.namespace().as_str(),
        )?)
    }

    /// Resolve the storage identity and declared dimensions together so guarded
    /// SQL publication agrees with VectorStore, including built-in aliases.
    pub(crate) fn vector_model_metadata(&self, model_name: &str) -> RuntimeResult<(String, usize)> {
        if request_excludes_embedder(model_name) {
            return Err(crate::RuntimeError::UnknownModel(model_name.to_string()));
        }
        let registry = self
            .embedder_registry
            .read()
            .map_err(|_| crate::RuntimeError::Internal("embedder registry lock poisoned".into()))?;
        if let Some(model) = parse_embedding_model_alias(model_name) {
            // Only proceed via the lattice path if this model is actually in the
            // registry; otherwise fall through to the custom-provider path.
            let key = model.to_string();
            if registry.contains(&key) {
                return Ok((key, model.dimensions()));
            }
        }
        registry
            .get_provider(model_name)
            .map(|provider| (model_name.to_owned(), provider.dimensions()))
            .ok_or_else(|| crate::RuntimeError::UnknownModel(model_name.to_string()))
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
        let namespace = token.namespace().as_str();
        {
            let cached = self.named_vector_stores.read().map_err(|_| {
                RuntimeError::Internal("named vector store cache lock poisoned".into())
            })?;
            if let Some(entry) = cached.get(identity.model_key()) {
                check_cached_named_vector_identity(&entry.identity, identity)?;
                if let Some(store) = entry.by_namespace.get(namespace) {
                    return Ok(Arc::clone(store));
                }
            }
        }
        let store = self.backend.vectors_for_namespace(
            identity.model_key(),
            identity.model_name(),
            identity.dimensions(),
            namespace,
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

        let mut cached = self
            .named_vector_stores
            .write()
            .map_err(|_| RuntimeError::Internal("named vector store cache lock poisoned".into()))?;
        let entry = cached
            .entry(identity.model_key().to_owned())
            .or_insert_with(|| CachedNamedVectorStores {
                identity: identity.clone(),
                by_namespace: HashMap::new(),
            });
        check_cached_named_vector_identity(&entry.identity, identity)?;
        entry
            .by_namespace
            .insert(namespace.to_owned(), Arc::clone(&store));
        Ok(store)
    }

    /// Output dimensions for a named embedding model, resolved from the
    /// embedder registry alone — no storage access. Mirrors
    /// [`vectors_for_model`](Self::vectors_for_model)'s resolution order:
    /// lattice aliases route through the enum when registered, otherwise the
    /// custom provider's declared `dimensions()`. `None` when no such model
    /// is registered.
    pub fn embedder_dimensions(&self, model_name: &str) -> Option<usize> {
        if request_excludes_embedder(model_name) {
            return None;
        }
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
                Err(crate::RuntimeError::permission_denied("authorize", reason))
            }
            Ok(_) => Err(crate::RuntimeError::permission_denied(
                "authorize",
                "gate denied",
            )),
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
                // The primary check authorizes writes to `primary` only. Each
                // extra namespace grants read visibility, so each one takes
                // its own Read-classified gate check before it may enter the
                // minted set — a token must never carry visibility the gate
                // was not asked about. Any deny or gate error refuses the
                // whole mint, naming the offending namespace (fail-closed).
                for extra in &extra_visible {
                    let extra_req = GateRequest::new(
                        actor.clone(),
                        extra.clone(),
                        "authorize.visible",
                        serde_json::Value::Null,
                    );
                    match self.config.gate.check(&extra_req) {
                        Ok(ref extra_decision) if extra_decision.is_allow() => {}
                        Ok(khive_gate::GateDecision::Deny { reason }) => {
                            return Err(crate::RuntimeError::permission_denied(
                                "authorize",
                                format!(
                                    "visibility namespace {:?} denied: {reason}",
                                    extra.as_str()
                                ),
                            ));
                        }
                        Ok(_) => {
                            return Err(crate::RuntimeError::permission_denied(
                                "authorize",
                                format!("visibility namespace {:?} denied by gate", extra.as_str()),
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                namespace = %extra.as_str(),
                                error = %crate::secret_gate::bounded_masked_log_text(&e.to_string()),
                                "authorize_with_visibility: extra-namespace gate check failed (fail-closed)"
                            );
                            return Err(crate::RuntimeError::Internal(format!(
                                "gate error: {}",
                                e.wire_reason()
                            )));
                        }
                    }
                }
                Ok(NamespaceToken::mint_with_visibility(
                    primary,
                    extra_visible,
                    actor,
                ))
            }
            Ok(khive_gate::GateDecision::Deny { reason }) => {
                Err(crate::RuntimeError::permission_denied("authorize", reason))
            }
            Ok(_) => Err(crate::RuntimeError::permission_denied(
                "authorize",
                "gate denied",
            )),
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
        // The mode gate must sit on THIS seam, not only on `install_blob_store`:
        // `BlobHydrator::new` is public, so without it a caller pairs a writable
        // store, installs it here, and `blob_store()` hands mutating pack paths
        // a writable store on a runtime whose declared mode is read-only.
        // Boot paths installing one hydrator across handles of MIXED modes —
        // where the blob pack's own backend mode, not each receiving
        // handle's, governs mutability — go through
        // [`Self::install_shared_blob_hydrator`] instead.
        if self.is_read_only() && !hydrator.enforces_read_only() {
            return Err(RuntimeError::InvalidInput(
                "this runtime is read-only: install the raw store with install_blob_store, \
                 which wraps it so every physical mutator refuses"
                    .to_string(),
            ));
        }
        self.install_blob_hydrator_slot(hydrator)
    }

    /// Install a boot-shared hydrator whose mutability is governed by the
    /// blob runtime's own mode, not this handle's domain-store mode.
    ///
    /// ADR-160 D3 installs one hydrator `Arc` on every runtime handle a boot
    /// produces, and the documented multi-backend matrix includes a writable
    /// blob secondary beside a read-only main: there the shared hydrator is
    /// legitimately writable on a read-only domain handle. The mode decision
    /// must therefore already be encoded in the hydrator, and it must have
    /// been DERIVED, not declared: only hydrators built through
    /// [`crate::BlobHydrator::resolve_for_governing_backend`] — whose mode
    /// comes from the governing backend's own access mode — are accepted
    /// here. A hand-paired hydrator (`BlobHydrator::new` / `for_mode`) is
    /// refused so a safe downstream caller cannot use this seam to put a
    /// writable store on a read-only runtime; such callers use
    /// [`Self::install_blob_hydrator`], which holds hydrator mode against
    /// this runtime's own.
    ///
    /// This gate is a wrong-wiring guard, not an in-process sandbox: which
    /// backend governs is the boot host's topology assertion, and a caller
    /// who deliberately selects an unrelated writable backend as governing
    /// is outside what any runtime seam can enforce (see the trust-model
    /// note on [`crate::BlobHydrator::resolve_for_governing_backend`]).
    pub fn install_shared_blob_hydrator(
        &self,
        hydrator: Arc<crate::blob::BlobHydrator>,
    ) -> RuntimeResult<()> {
        if !hydrator.is_governed() {
            return Err(RuntimeError::InvalidInput(
                "the shared install seam accepts only hydrators whose mode was derived from a \
                 governing backend (BlobHydrator::resolve_for_governing_backend); use \
                 install_blob_hydrator for a hand-paired hydrator"
                    .to_string(),
            ));
        }
        if hydrator.budget_bytes() != self.config.blob_hydration_bytes {
            return Err(RuntimeError::InvalidInput(format!(
                "blob hydrator budget {} does not match this runtime's resolved budget {}",
                hydrator.budget_bytes(),
                self.config.blob_hydration_bytes
            )));
        }
        self.install_blob_hydrator_slot(hydrator)
    }

    /// Whether `candidate` is the same pairing as the installed `current`:
    /// the exact `Arc`, or a distinct hydrator allocation over the same raw
    /// store with the same budget and mode. The latter arises when two
    /// concurrent first installs each construct a hydrator from one raw
    /// store — the `OnceLock` loser must read as idempotent, not as a
    /// conflicting install.
    fn is_same_blob_pairing(
        current: &Arc<crate::blob::BlobHydrator>,
        candidate: &Arc<crate::blob::BlobHydrator>,
    ) -> bool {
        Arc::ptr_eq(current, candidate)
            || (Arc::ptr_eq(&current.raw_store(), &candidate.raw_store())
                && current.budget_bytes() == candidate.budget_bytes()
                && current.enforces_read_only() == candidate.enforces_read_only())
    }

    /// One-shot slot semantics shared by both install seams: first install
    /// wins, an equivalent pairing is idempotent, a different pairing is
    /// refused (replacing it would split or reset the aggregate admission
    /// budget while requests may still hold leases).
    fn install_blob_hydrator_slot(
        &self,
        hydrator: Arc<crate::blob::BlobHydrator>,
    ) -> RuntimeResult<()> {
        if let Some(current) = self.blob_hydrator.get() {
            return if Self::is_same_blob_pairing(current, &hydrator) {
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
                if Self::is_same_blob_pairing(current, &candidate) {
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
            // Reinstalling the same raw store is idempotent in BOTH modes:
            // on a read-only runtime the installed hydrator wraps the raw
            // store, so identity is checked against the raw handle the
            // hydrator remembers, not only the (possibly wrapped) paired one.
            let current_store = current.store();
            if Arc::ptr_eq(&current_store, &store) || Arc::ptr_eq(&current.raw_store(), &store) {
                return Ok(());
            }
        }
        // A read-only runtime holds its mode at this seam, not only during
        // boot resolution: an arbitrary store installed after launch is
        // wrapped so every physical mutator refuses while the bounded read
        // surface stays available. Without this, post-boot installation is a
        // writable bypass of the runtime's declared mode.
        let hydrator = if self.is_read_only() {
            crate::blob::BlobHydrator::new_read_only(store, self.config.blob_hydration_bytes)?
        } else {
            crate::blob::BlobHydrator::new(store, self.config.blob_hydration_bytes)?
        };
        self.install_blob_hydrator(Arc::new(hydrator))
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

    /// Return the installed `BlobStore`, or `RuntimeError::Unconfigured` with
    /// the operator-facing message when none is installed.
    ///
    /// Packs share this conversion so an operator sees one error wherever the
    /// missing `[storage.blob]` configuration is first hit.
    pub fn require_blob_store(&self) -> RuntimeResult<Arc<dyn khive_storage::BlobStore>> {
        self.blob_store().ok_or_else(Self::no_blob_store)
    }

    /// Return the installed shared blob hydrator, or `RuntimeError::Unconfigured`
    /// with the same message as [`Self::require_blob_store`] when none is
    /// installed. The store is read from the hydrator, so both accessors are
    /// unset under the same condition.
    pub fn require_blob_hydrator(&self) -> RuntimeResult<Arc<crate::blob::BlobHydrator>> {
        self.blob_hydrator().ok_or_else(Self::no_blob_store)
    }

    fn no_blob_store() -> RuntimeError {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or \
             KHIVE_BLOB_ROOT)"
                .to_string(),
        )
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
            let prior = std::mem::take(&mut *guard);
            *guard = note_kinds
                .into_iter()
                .map(|name| NoteKindEntry {
                    embedding_policy: prior
                        .iter()
                        .find(|entry| entry.name == name)
                        .map(|entry| entry.embedding_policy)
                        .unwrap_or_default(),
                    name,
                    registered: true,
                })
                .collect();
        }
    }

    /// Install pack-declared embedding policy on the note-kind registry.
    /// The transport calls this for every runtime after pack registration.
    pub fn install_note_embedding_policies(&self, policies: &[crate::NoteEmbeddingPolicySpec]) {
        if let Ok(mut guard) = self.valid_note_kinds.write() {
            for spec in policies {
                if let Some(entry) = guard.iter_mut().find(|entry| entry.name == spec.kind) {
                    entry.embedding_policy = spec.policy;
                } else {
                    guard.push(NoteKindEntry {
                        name: spec.kind.to_owned(),
                        embedding_policy: spec.policy,
                        registered: false,
                    });
                }
            }
        }
    }

    /// Registered models selected by the installed embedding policy for a note kind.
    /// Unknown kinds retain the all-models default.
    pub fn embedding_models_for_note_kind(&self, kind: &str) -> Vec<String> {
        let policy = self
            .valid_note_kinds
            .read()
            .ok()
            .and_then(|guard| {
                guard
                    .iter()
                    .find(|entry| entry.name == kind)
                    .map(|entry| entry.embedding_policy)
            })
            .unwrap_or_default();
        let models = self.registered_embedding_model_names();
        match policy {
            crate::NoteEmbeddingPolicy::AllModels => models,
            crate::NoteEmbeddingPolicy::DefaultModel => {
                let default = self.default_embedder_name();
                models
                    .into_iter()
                    .filter(|name| name.as_str() == default)
                    .collect()
            }
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
        if !guard.iter().any(|entry| entry.registered) {
            return Ok(());
        }
        if guard
            .iter()
            .any(|entry| entry.registered && entry.name == kind)
        {
            Ok(())
        } else {
            let valid = guard
                .iter()
                .filter(|entry| entry.registered)
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(crate::RuntimeError::InvalidInput(format!(
                "unknown note kind {kind:?}; valid: {}",
                valid
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

    /// Clone read-side backend and embedder handles without retaining this
    /// runtime's pack callback slots. Providers stored in one of those slots
    /// must not hold a clone that points back to their own installation Arc,
    /// including indirectly through another hook's captured runtime.
    pub fn detached_for_note_search_ann_provider(&self) -> Self {
        let mut detached = self.clone();
        detached.note_search_ann_provider = Arc::new(RwLock::new(None));
        detached.note_mutation_hook = Arc::new(RwLock::new(None));
        detached.entity_type_validator = Arc::new(RwLock::new(None));
        detached.note_write_validator = Arc::new(RwLock::new(None));
        detached.entity_kind_hooks = Arc::new(RwLock::new(Vec::new()));
        detached.fusion_executors = Arc::new(RwLock::new(HashMap::new()));
        detached
    }

    /// Install the memory pack's note-search provider only on its own opened
    /// backend. A same-named but separate store keeps the exact search route.
    pub fn install_note_search_ann_provider(&self, provider: Arc<dyn NoteSearchAnnProvider>) {
        if !provider.serves_backend(self) {
            return;
        }
        if let Ok(mut guard) = self.note_search_ann_provider.write() {
            *guard = Some(provider);
        }
    }

    pub(crate) fn note_search_ann_provider(
        &self,
    ) -> RuntimeResult<Option<Arc<dyn NoteSearchAnnProvider>>> {
        self.note_search_ann_provider
            .read()
            .map(|guard| {
                guard
                    .as_ref()
                    .filter(|provider| provider.serves_backend(self))
                    .cloned()
            })
            .map_err(|_| RuntimeError::Internal("note-search ANN provider lock poisoned".into()))
    }

    /// Install the pack-aggregated entity-kind update hooks (issue #2943).
    ///
    /// Called by the transport after the `VerbRegistry` is built, same
    /// timing as [`install_kind_registry`](Self::install_kind_registry) —
    /// pass `registry.entity_kind_hooks()`. Idempotent: a later call
    /// replaces the set.
    pub fn install_entity_kind_hooks(&self, hooks: EntityKindHooks) {
        if let Ok(mut guard) = self.entity_kind_hooks.write() {
            *guard = hooks;
        }
    }

    /// The installed `KindHook` for entity `kind`, if its owning pack
    /// registered one via [`install_entity_kind_hooks`](Self::install_entity_kind_hooks).
    ///
    /// `None` before the transport installs the aggregate (bare runtime) or
    /// when no pack registered a hook for this entity kind — the caller
    /// treats this the same as a hook whose `validate_entity_update`
    /// inherited the trait's `Ok(())` default.
    pub(crate) fn entity_kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        self.entity_kind_hooks.read().ok().and_then(|guard| {
            guard
                .iter()
                .find(|(k, _)| k == kind)
                .map(|(_, hook)| hook.clone())
        })
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
    /// `message`-kind transport properties only when called through
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
    /// same registry entry. A pack can replace a built-in name while a cold
    /// service is initializing, so a second registry lookup would be unsafe.
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

// IN-CRATE TEST JUSTIFICATION: tests here cover KhiveRuntime construction helpers
// (in-memory backend wiring, NamespaceToken::for_namespace) that are
// pub(crate)-only and cannot be called from the integration test crate.
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
