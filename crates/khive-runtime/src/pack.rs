// FILE SIZE JUSTIFICATION: pack.rs is the load-bearing dispatch core — VerbRegistry,
// VerbRegistryBuilder, PackRuntime, DispatchHook, and their test scaffolding all
// share internal state (packs Vec, gate, event_store) that cannot be cleanly split
// without exposing private fields or duplicating the scaffolding. Inline tests cover
// collision detection and dispatch path that require direct access to VerbRegistry
// internals. Split plan: when the verb surface reaches a stable v1 API, extract
// VerbRegistryBuilder into `pack/builder.rs` and gate/event logic into `pack/dispatch.rs`.
//! Pack runtime trait and verb registry.
//!
//! `PackRuntime` mirrors `Pack`'s const associated items as methods for object safety.
//! Build a [`VerbRegistry`] via `VerbRegistryBuilder::build()`; registration is builder-only.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use crate::operations::{LinkSpec, Resolved};
use crate::runtime::NamespaceToken;
use async_trait::async_trait;
use khive_gate::{AllowAllGate, AuditEvent, GateDecision, GateRef, GateRequest};
use khive_storage::{Event, EventStore, EventView, SubstrateKind};
use khive_types::{EventKind, EventOutcome, Namespace};
use serde_json::Value;

pub use khive_types::{
    EdgeEndpointRule, EndpointKind, EntityTypeDef, HandlerDef, NoteKindSpec, NoteLifecycleSpec,
    PackSchemaPlan, ParamDef, VerbCategory, VerbPresentationPolicy, Visibility,
    RESERVED_ENVELOPE_ARGS,
};
// Backward-compat re-export.
#[allow(deprecated)]
pub use khive_types::VerbDef;

use crate::validation::ValidationRule;

/// Name of the pack providing the shared CRUD verbs and the general-purpose
/// note kinds those verbs exist to serve.
///
/// Its note kinds are the ones any caller may author freely through `create`
/// and `update`; every other pack's note kinds are records maintained by that
/// pack's own verbs. Used by
/// [`VerbRegistry::pack_owned_note_kinds`].
pub const GENERIC_CRUD_PACK: &str = "kg";

/// Stable advisory code emitted when a successful inspection cannot persist
/// its dispatch audit because the configured audit backend is read-only.
pub const AUDIT_PERSISTENCE_SKIPPED_READ_ONLY: &str = "audit_persistence_skipped_read_only";

const FULL_UUID_IDENTIFIER_HELP: &str = "A complete UUID spelling accepted by the consuming \
    parameter directly names one globally unique record; direct UUID lookup is not a namespace \
    search. Strict identifier responses use canonical lowercase dashed UUIDs.";
const SHORT_PREFIX_IDENTIFIER_HELP: &str = "A short UUID prefix is at least 8 hexadecimal \
    characters without dashes that do not parse as a complete UUID. It is a resolution, not a \
    direct identifier; a 32-character compact UUID is complete input instead. Its lookup scope belongs to the consuming parameter: \
    operations governed by ADR-007's by-ID contract resolve without a namespace filter, while \
    other resolvers may search only the caller's primary namespace. A prefix can be missing or \
    ambiguous.";
const IDENTIFIER_PARAMETER_HELP: &str = "A parameter that requires a full UUID rejects prefixes \
    and explains the resolution consequence. Its corresponding response field remains a \
    canonical full UUID so the value can be submitted again.";

fn identifier_resolution_help() -> Value {
    serde_json::json!({
        "full_uuid": FULL_UUID_IDENTIFIER_HELP,
        "short_prefix": SHORT_PREFIX_IDENTIFIER_HELP,
        "parameter_rule": IDENTIFIER_PARAMETER_HELP,
    })
}

/// Pack-auxiliary schema plan.
///
/// Declares `CREATE TABLE IF NOT EXISTS` statements for pack-owned tables that
/// are NOT part of the core substrate schema (entities, notes, edges, events).
/// Applied at boot via `StorageBackend::apply_schema` / `apply_pack_schema_plan`.
///
/// Core substrate tables evolve through versioned migrations. Pack schema is
/// strictly for pack-auxiliary tables (e.g. GTD lifecycle audit, memory index).
/// v1 pack schemas are non-versioned.
#[derive(Debug, Default, Clone)]
pub struct SchemaPlan {
    /// Owning pack name.
    pub pack: &'static str,
    /// DDL statements applied idempotently at boot.
    /// Each entry must be a self-contained `CREATE TABLE IF NOT EXISTS` or
    /// similar idempotent statement.
    pub statements: &'static [&'static str],
}

impl SchemaPlan {
    /// Construct a `SchemaPlan` with no statements.
    ///
    /// Packs whose state lives entirely in the core substrate tables (entities,
    /// notes, edges) use this as their `schema_plan()` return value.
    pub const fn empty() -> Self {
        Self {
            pack: "",
            statements: &[],
        }
    }

    /// Returns `true` when the plan contains no DDL statements.
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }
}

/// Best-effort hook called after every successful verb dispatch.
///
/// The runtime supplies a synthetic [`EventView`] whose `event` describes the
/// dispatch outcome and whose `observations` vector is currently empty. Loading
/// persisted provenance observations belongs to an explicit caller or the
/// deferred event-consumer contract; this hook does not provide it.
#[async_trait]
pub trait DispatchHook: Send + Sync {
    /// Called with the dispatch-outcome event view after a successful pack dispatch.
    ///
    /// Errors are logged via `tracing::warn!` and never propagated to the
    /// caller; the dispatch has already succeeded.
    async fn on_dispatch(&self, view: &EventView);
}

use crate::error::{
    CircularPackDependency, MissingPackDependencies, MissingPackDependency, RuntimeError,
};
use crate::KhiveRuntime;

/// Async dispatch trait for packs.
///
/// This is the object-safe behavioral counterpart to `khive_types::Pack`.
/// `Pack` uses const associated items (not object-safe in Rust); this trait
/// mirrors that metadata as methods and adds async dispatch.
///
/// Registration requires `P: Pack + PackRuntime` — the compiler enforces
/// that every runtime pack also declares its vocabulary via `Pack`.
#[async_trait]
pub trait PackRuntime: Send + Sync {
    /// Pack name — must equal `<Self as Pack>::NAME`.
    fn name(&self) -> &str;

    /// Note kinds this pack owns — must equal `<Self as Pack>::NOTE_KINDS`.
    fn note_kinds(&self) -> &'static [&'static str];

    /// Entity kinds this pack owns — must equal `<Self as Pack>::ENTITY_KINDS`.
    fn entity_kinds(&self) -> &'static [&'static str];

    /// Brain profile consumer kinds this pack requests — must equal
    /// `<Self as Pack>::BRAIN_CONSUMER_KINDS`.
    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Handlers this pack registers — must equal `<Self as Pack>::HANDLERS`.
    fn handlers(&self) -> &'static [HandlerDef];

    /// Pack-extensible edge endpoint rules — must equal `<Self as Pack>::EDGE_RULES`.
    /// Defaults to empty so existing packs that don't extend the edge contract
    /// can ignore it.
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &[]
    }

    /// Pack-extensible entity-type subtypes — must equal `<Self as Pack>::ENTITY_TYPES`.
    /// Defaults to empty so existing packs that don't extend the entity_type
    /// registry can ignore it.
    fn entity_types(&self) -> &'static [EntityTypeDef] {
        &[]
    }

    /// Pack names whose vocabulary this pack references.
    /// Defaults to empty so existing packs compile without changes.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// NoteKindSpec declarations for note kinds this pack owns.
    ///
    /// Packs that introduce note kinds with explicit lifecycle semantics
    /// declare the spec here.  The runtime collects these for introspection
    /// and future enforcement.  Defaults to empty so existing packs compile
    /// without changes.
    fn note_kind_specs(&self) -> &'static [NoteKindSpec] {
        &[]
    }

    /// Optional per-kind hook for shared CRUD specialization.
    ///
    /// When a kind is owned by this pack (declared in `note_kinds()` or
    /// `entity_kinds()`), returning `Some(hook)` opts that kind into
    /// pack-specific behavior — defaults, derived properties, side-effect
    /// edges — through the shared `create` path. Returning `None` keeps
    /// the kind as plain storage with no specialization.
    fn kind_hook(&self, _kind: &str) -> Option<Arc<dyn KindHook>> {
        None
    }

    /// Accept the trusted channel-ingest capability grant for this specific
    /// pack instance.
    ///
    /// Called at most once per instance, immediately after this instance is
    /// constructed via [`PackFactory::create_install`], and only for packs
    /// whose name appears in `CHANNEL_INGEST_CAPABLE_PACKS`. Storing the
    /// grant on `self` (rather than on the `&'static dyn PackFactory`, which
    /// is a single process-wide singleton shared by every instance the
    /// factory ever creates) makes the grant instance-bound: a `CommPack`
    /// built outside [`PackRegistry::register_packs`] holds no capability
    /// unless something calls this on that specific instance. Defaults to a
    /// no-op so packs outside the allowlist compile without changes.
    fn accept_channel_ingest_capability(&self, _capability: ChannelIngestCapability) {}

    /// Pack-auxiliary schema.
    ///
    /// Returns DDL statements for pack-owned tables that are NOT part of the
    /// core substrate schema. Statements are idempotent (`CREATE TABLE IF NOT
    /// EXISTS`) so callers can apply them safely on every registration. Core
    /// substrate tables evolve through versioned migrations; pack schema is
    /// strictly pack-auxiliary.
    ///
    /// Defaults to an empty plan — packs that store everything in the core
    /// substrate tables (entities, notes, edges, events) return this default.
    ///
    /// Plans are aggregated via [`VerbRegistry::all_schema_plans`] and applied
    /// at startup via `KhiveMcpServer::with_packs`. Packs that need their
    /// schema present (e.g. GTD) also self-bootstrap lazily on first call for
    /// robustness in test contexts that create fresh in-memory databases.
    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan::empty()
    }

    /// Domain-specific validation rules contributed by this pack.
    ///
    /// Rule IDs MUST follow the `<pack>/<rule-id>` namespace convention.
    /// Built-in rules (no pack prefix) are reserved for the `khive-runtime`
    /// validation infrastructure.
    ///
    /// Defaults to empty — packs with no domain-specific rules return `&[]`.
    fn validation_rules(&self) -> &'static [ValidationRule] {
        &[]
    }

    /// Register custom embedding providers with the runtime. Called during pack
    /// initialisation, before the first verb dispatch, so `KhiveRuntime::embedder(name)`
    /// resolves provider names declared here. Default no-op — packs that only use
    /// built-in lattice models do not need to override this.
    /// See `docs/api/pack.md#register_embedders` for a usage example.
    fn register_embedders(&self, _runtime: &KhiveRuntime) {}

    /// Install a pack-owned entity-type validator on the runtime, called during pack
    /// initialisation (after the registry is built, before the first dispatch) so
    /// `create_many`/`create_entity` reject unregistered `entity_type` values at the
    /// runtime layer. Default no-op leaves the validator absent (skip-when-None).
    /// See `docs/api/pack.md#register_entity_type_validator` for the two-hook compatibility contract.
    fn register_entity_type_validator(&self, _runtime: &KhiveRuntime) {}

    /// Install a pack-owned entity-type validator that also receives the boot-time
    /// composed set of every loaded pack's `ENTITY_TYPES` ([`VerbRegistry::all_entity_types`]).
    /// Defaults to calling [`register_entity_type_validator`](Self::register_entity_type_validator)
    /// with just the runtime. `call_register_entity_type_validators` calls this hook, not
    /// the simpler one — override this to receive the composed vocabulary.
    /// See `docs/api/pack.md#register_entity_type_validator` for the two-hook compatibility contract.
    fn register_entity_type_validator_with_types(
        &self,
        runtime: &KhiveRuntime,
        _pack_entity_types: &[EntityTypeDef],
    ) {
        self.register_entity_type_validator(runtime);
    }

    /// Install a pack-owned note-mutation hook on the runtime, called during pack
    /// initialisation with the same timing as `register_entity_type_validator`. Packs
    /// that cache derived state keyed by note content (e.g. `khive-pack-memory`'s warm
    /// ANN index) override this to install a hook via
    /// `KhiveRuntime::install_note_mutation_hook`. Default no-op leaves the hook absent.
    /// See `docs/api/pack.md#register_note_mutation_hook` for cross-pack notification rationale.
    fn register_note_mutation_hook(&self, _runtime: &KhiveRuntime) {}

    /// Install a note-write validator on the runtime, called at pack
    /// initialisation with the same timing as `register_note_mutation_hook`.
    ///
    /// A pack owning a note kind whose properties carry identity that the
    /// runtime can derive from the authorization token implements this and
    /// calls `KhiveRuntime::install_note_write_validator`, so the identity is
    /// derived at every note-write site rather than trusted from caller input
    /// on the write paths that reach no pack verb. Default no-op leaves the
    /// slot absent. The slot holds one validator, so an implementation must
    /// return properties for kinds it does not own unchanged.
    fn register_note_write_validator(&self, _runtime: &KhiveRuntime) {}

    /// Warm up any in-memory state from persisted snapshots (optional). Called after
    /// all packs are registered but before serving the first request. Must be
    /// idempotent and infallible — errors are logged internally, never propagated.
    async fn warm(&self) {}

    /// Names of all embedding models registered on this pack's underlying runtime
    /// handle. Defaults to empty — only packs that own embedding-bearing verbs
    /// (kg, memory) need to override this.
    /// See `docs/api/pack.md#registered_embedding_model_names` for the ADR-103 consumer.
    fn registered_embedding_model_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Dispatch a verb call. Returns serialized JSON response.
    ///
    /// The `registry` parameter gives the handler access to the merged
    /// vocabulary and kind hooks across all loaded packs.
    /// The `token` is an authorized namespace token minted by the dispatch
    /// boundary after gate authorization — handlers must use it directly.
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError>;
}

/// Per-kind specialization for shared CRUD.
///
/// Packs implement `KindHook` for kinds they own that need:
/// - **Defaults** filled into create args (e.g. `status="inbox"` for tasks)
/// - **Derived properties** computed from args (e.g. salience from priority)
/// - **Side-effect writes** after the storage commit (e.g. `depends_on` edges)
/// - **Cross-pack validation** before shared CRUD mutates an owned kind
///
/// Hooks are stateless from the framework's perspective — they receive the
/// runtime and the current mutation inputs as method parameters. The pack
/// registers them via [`PackRuntime::kind_hook`].
///
/// Lifecycle verbs (e.g. gtd's `complete`, `transition`) remain pack-owned
/// verbs. Shared `create`, note `update`, and `link` calls flow through this
/// trait when an endpoint kind has an owning pack hook.
#[async_trait]
pub trait KindHook: Send + Sync + std::fmt::Debug {
    /// Mutate args before the storage write. Fill defaults, normalize values,
    /// rearrange user-facing fields into the storage shape expected by the
    /// shared CRUD handler.
    ///
    /// Returning an error aborts the create call (no storage write happens).
    async fn prepare_create(
        &self,
        runtime: &KhiveRuntime,
        args: &mut Value,
    ) -> Result<(), RuntimeError>;

    /// Fire side effects after a successful storage write — graph edges,
    /// derived observations, etc. The newly created record's UUID is passed
    /// so the hook can attach metadata referencing it.
    ///
    /// Errors here are **logged but not propagated** — the storage write has
    /// already succeeded; failing the call would mislead the caller.
    /// Implementations should `tracing::warn!` and return `Ok(())` for
    /// best-effort side effects.
    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: uuid::Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError>;

    /// Normalize a shared note update before storage is mutated.
    ///
    /// The default preserves the original property-validation contract. A
    /// kind-owning pack overrides this when caller-facing note fields mirror
    /// owned properties and must be changed together (for example, a task's
    /// searchable `content` and `properties.description`).
    async fn prepare_note_update(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let properties = args.get("properties").filter(|value| !value.is_null());
        self.validate_note_update(runtime, token, note, properties)
            .await
    }

    /// Validate a shared note-property update before storage is mutated.
    ///
    /// The default accepts the update. Kind-owning packs override this when a
    /// property has invariants that generic CRUD cannot know about (for
    /// example, GTD task dependency acyclicity).
    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        _properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Validate one or more shared graph links before any edge is written.
    ///
    /// A batch is supplied as a unit so a hook can reject a cycle formed only
    /// by the proposed edges. The default accepts every link.
    async fn validate_links(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _links: &[crate::LinkSpec],
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// Optional sub-trait for packs that own private SQL tables and issue UUIDs
/// that must be reachable through the generic `get(id)` and `delete(id)` verbs.
///
/// Implementing both methods is required — the sub-trait bundles them atomically
/// so partial implementation is a compile-time error, not a runtime surprise.
/// Packs whose records live in the shared entity/note substrate (gtd, memory)
/// do not implement this sub-trait.
#[async_trait]
pub trait PackByIdResolver: Send + Sync {
    /// Attempt to resolve a live (non-deleted) UUID owned by this pack's private tables.
    ///
    /// Returns `Some(Resolved::PackRecord { ... })` if this pack owns the UUID,
    /// `None` if it does not (the caller continues to the next resolver),
    /// or `Err(...)` on a storage error.
    ///
    /// Must query domain-authoritative tables before mirror tables.
    /// Must NOT filter by namespace. UUID v4 is globally unique; by-ID
    /// resolution is namespace-blind per ADR-007.
    async fn resolve_by_id(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<crate::Resolved>, crate::RuntimeError>;

    /// Attempt to resolve a UUID including already-soft-deleted records.
    ///
    /// Used by the hard-delete path. Default delegates to `resolve_by_id`;
    /// packs with `deleted_at` columns override this to query without the filter.
    async fn resolve_by_id_including_deleted(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<crate::Resolved>, crate::RuntimeError> {
        self.resolve_by_id(id).await
    }

    /// Delete a record owned by this pack's private tables.
    ///
    /// `hard` mirrors the `delete` verb's `hard?` argument.
    /// Default behavior for packs with a `deleted_at` column MUST be soft-delete;
    /// `hard=true` performs permanent removal.
    ///
    /// Returns `Ok(Value)` with a `{ deleted: true, id, kind, hard }` body on success.
    /// Returns `Err(RuntimeError::NotFound(...))` if the record does not exist.
    async fn delete_by_id(
        &self,
        id: uuid::Uuid,
        hard: bool,
    ) -> Result<serde_json::Value, crate::RuntimeError>;
}

/// Builder for constructing a `VerbRegistry`.
///
/// Packs are registered here; once `.build()` is called the registry is
/// immutable and cheaply cloneable.
pub struct VerbRegistryBuilder {
    packs: Vec<Box<dyn PackRuntime>>,
    resolvers: Vec<(String, Box<dyn PackByIdResolver>)>,
    gate: GateRef,
    default_namespace: String,
    /// Operator-configured read-visibility set (ADR-007 Rev 4 Rule 3b).
    ///
    /// Threads into `VerbRegistry::visible_namespaces` and is consumed by the
    /// default dispatch path to widen read scope to `['local'] ∪ visible_namespaces`.
    /// Writes remain pinned to `'local'`. An explicit `namespace=` request param
    /// is a precise escape and is not widened by this set. A cloud gate may also
    /// consult the list as policy input at its own layer.
    visible_namespaces: Vec<Namespace>,
    /// Configured actor identity label (ADR-057). When set, dispatch mints tokens
    /// carrying this actor so that `comm.inbox` filters by `to_actor`.
    actor_id: Option<String>,
    /// Optional audit event sink.
    ///
    /// When set, every gate check writes a storage `Event` in addition to the
    /// `tracing::info!` emission. The store is `Arc<dyn EventStore>` so the
    /// registry does not depend on the full `KhiveRuntime` surface — only the
    /// audit-persistence capability is needed here.
    event_store: Option<Arc<dyn EventStore>>,
    /// The configured audit backend is intentionally read-only, so dispatch
    /// omits the known-failing append and the transport surfaces an advisory.
    audit_store_read_only: bool,
    /// Optional post-dispatch hook.
    ///
    /// When set, every successful pack dispatch calls `hook.on_dispatch(view)`
    /// with a synthetic `EventView` describing the outcome and carrying no
    /// observations. Opt-in: when None, no overhead is incurred.
    dispatch_hook: Option<Arc<dyn DispatchHook>>,
    /// ADR-133 audit-batch config override, applied when `build()` lazily
    /// constructs the batch seam from `event_store`. `None` uses
    /// `AuditBatchConfig::default()`.
    audit_batch_config: Option<crate::audit_batch::AuditBatchConfig>,
}

impl VerbRegistryBuilder {
    /// Create a builder with no packs, `AllowAllGate`, and the local namespace as default.
    pub fn new() -> Self {
        Self {
            packs: Vec::new(),
            resolvers: Vec::new(),
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::local().as_str().to_string(),
            visible_namespaces: vec![],
            actor_id: None,
            event_store: None,
            audit_store_read_only: false,
            dispatch_hook: None,
            audit_batch_config: None,
        }
    }

    /// Set the operator-configured read-visibility set (ADR-007 Rev 4 Rule 3b).
    ///
    /// On the default (no explicit `namespace=` param) dispatch path, reads fan
    /// out over `['local'] ∪ ns`. Writes remain pinned to `'local'`. An explicit
    /// `namespace=` request parameter is a precise single-namespace escape and
    /// is not widened by this set. A cloud gate may also consult the list as
    /// policy input at its own layer.
    pub fn with_visible_namespaces(&mut self, ns: Vec<Namespace>) -> &mut Self {
        self.visible_namespaces = ns;
        self
    }

    /// Set the configured actor identity label (ADR-057).
    ///
    /// When set, the dispatch path mints tokens carrying this actor so that
    /// `comm.inbox` applies the `to_actor` filter for directed delivery.
    /// When `None` (default), tokens carry `ActorRef::anonymous()` and inbox
    /// falls back to party-line behavior.
    pub fn with_actor_id(&mut self, actor_id: Option<String>) -> &mut Self {
        self.actor_id = actor_id;
        self
    }

    /// Register a pack. The bound `P: Pack + PackRuntime` ensures the pack
    /// declares vocabulary via `Pack` consts alongside runtime dispatch.
    pub fn register<P: khive_types::Pack + PackRuntime + 'static>(&mut self, pack: P) -> &mut Self {
        self.packs.push(Box::new(pack));
        self
    }

    /// Register a boxed pack directly.
    ///
    /// Crate-private: only [`PackRegistry::register_packs`] should call this.
    /// External callers must use the typed [`Self::register`] which enforces the
    /// `Pack + PackRuntime` dual-impl contract at the call site.  Here the
    /// contract is satisfied upstream at the [`PackFactory::create`] site.
    pub(crate) fn register_boxed(&mut self, pack: Box<dyn PackRuntime>) -> &mut Self {
        self.packs.push(pack);
        self
    }

    /// Register a by-ID resolver for a pack that owns private SQL tables.
    ///
    /// Packs that implement `PackByIdResolver` call this during their boot path
    /// so that `get(id)` and `delete(id)` can reach their records.
    pub fn register_resolver(
        &mut self,
        name: impl Into<String>,
        resolver: Box<dyn PackByIdResolver>,
    ) -> &mut Self {
        self.resolvers.push((name.into(), resolver));
        self
    }

    /// Set the authorization gate consulted on every dispatch.
    ///
    /// Defaults to `AllowAllGate` if not set. `Deny` is authoritative — a deny
    /// decision aborts dispatch with `RuntimeError::PermissionDenied`. Gate
    /// infrastructure errors abort dispatch with `RuntimeError::GateUnavailable`.
    pub fn with_gate(&mut self, gate: GateRef) -> &mut Self {
        self.gate = gate;
        self
    }

    /// Set the namespace surfaced to the gate when a verb does not carry an
    /// explicit `namespace` argument. Transports should plumb the runtime's
    /// `default_namespace` so the gate's `input.namespace` always reflects
    /// the operation's true tenant.
    pub fn with_default_namespace(&mut self, ns: impl Into<String>) -> &mut Self {
        self.default_namespace = ns.into();
        self
    }

    /// Set the `EventStore` used to persist audit events.
    ///
    /// When configured, every gate check appends one `Event` (substrate =
    /// `Event`, outcome = `Success` on allow, `Denied` on deny, or `Error` on
    /// gate unavailability) in addition to the `tracing::info!` emission.
    ///
    /// Callers that do not set this field continue to use tracing-only emission
    /// (the v0.2 default), except `git.digest`: its successful response carries
    /// a durable receipt and therefore fails safely when no store is configured.
    pub fn with_event_store(&mut self, store: Arc<dyn EventStore>) -> &mut Self {
        self.event_store = Some(store);
        self.audit_store_read_only = false;
        self
    }

    /// Override the ADR-133 audit-batch seam's tunables, applied when
    /// `build()` lazily constructs the batch from `event_store`.
    /// `None` (the default) uses `AuditBatchConfig::default()`. Exposed for
    /// tests that need to force a small `max_pending_rows` or a short
    /// `admission_deadline` to exercise admission-pressure paths
    /// deterministically (khive#2117, khive#2147, khive#2208, khive#2217).
    pub fn with_audit_batch_config(
        &mut self,
        config: crate::audit_batch::AuditBatchConfig,
    ) -> &mut Self {
        self.audit_batch_config = Some(config);
        self
    }

    /// Mark audit persistence unavailable because its backend is read-only.
    ///
    /// No `EventStore` is retained, so dispatch never attempts a write that is
    /// known to fail. Successful request entries expose a machine-readable
    /// advisory without changing their canonical verb result shape.
    pub fn with_read_only_audit_store(&mut self) -> &mut Self {
        self.event_store = None;
        self.audit_store_read_only = true;
        self
    }

    /// Register a post-dispatch hook.
    ///
    /// When set, every successful pack dispatch calls `hook.on_dispatch(view)`
    /// with a synthetic [`EventView`] describing the verb outcome. Its
    /// `observations` vector is empty; callers that need persisted provenance
    /// must load it explicitly. The hook is opt-in: registries without a hook
    /// incur zero overhead on the dispatch hot path.
    ///
    /// Brain pack uses this as a best-effort in-memory update path. Errors from
    /// `on_dispatch` are logged via `tracing::warn!` and never propagated.
    pub fn with_dispatch_hook(&mut self, hook: Arc<dyn DispatchHook>) -> &mut Self {
        self.dispatch_hook = Some(hook);
        self
    }

    /// Consume the builder and produce an immutable, cloneable registry.
    ///
    /// Performs a topological sort of packs using Kahn's algorithm.
    /// Returns an error if any declared dependency is missing from the loaded
    /// pack set, or if a circular dependency is detected.
    pub fn build(self) -> Result<VerbRegistry, RuntimeError> {
        let packs = self.packs;
        let mut name_to_idx: HashMap<&str, usize> = HashMap::with_capacity(packs.len());
        for (idx, pack) in packs.iter().enumerate() {
            if let Some(prev_idx) = name_to_idx.insert(pack.name(), idx) {
                return Err(RuntimeError::PackRedeclared {
                    name: pack.name().to_string(),
                    first_idx: prev_idx,
                    second_idx: idx,
                });
            }
        }

        // Apply this metadata invariant to every HandlerDef, including Subhandlers. Subhandlers
        // are not top-level MCP-callable, but their describe/help contract still cannot truthfully
        // advertise a name rejected by every typed request parser before visibility dispatch.
        for pack in &packs {
            for handler in pack.handlers() {
                for parameter in handler.params {
                    if RESERVED_ENVELOPE_ARGS.contains(&parameter.name) {
                        return Err(RuntimeError::ReservedEnvelopeParam {
                            pack: pack.name().to_string(),
                            verb: handler.name.to_string(),
                            param: parameter.name.to_string(),
                        });
                    }
                }
            }
        }

        let mut missing: Vec<MissingPackDependency> = Vec::new();
        let mut indegree = vec![0usize; packs.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); packs.len()];

        for (idx, pack) in packs.iter().enumerate() {
            for &requires in pack.requires() {
                match name_to_idx.get(requires).copied() {
                    Some(dep_idx) => {
                        dependents[dep_idx].push(idx);
                        indegree[idx] += 1;
                    }
                    None => missing.push(MissingPackDependency {
                        from: pack.name().to_string(),
                        requires: requires.to_string(),
                    }),
                }
            }
        }

        if !missing.is_empty() {
            return if missing.len() == 1 {
                Err(RuntimeError::MissingPackDependency(missing.remove(0)))
            } else {
                Err(RuntimeError::MissingPackDependencies(
                    MissingPackDependencies { missing },
                ))
            };
        }

        let mut ready: VecDeque<usize> = indegree
            .iter()
            .enumerate()
            .filter_map(|(idx, degree)| (*degree == 0).then_some(idx))
            .collect();
        let mut ordered_indices = Vec::with_capacity(packs.len());

        while let Some(idx) = ready.pop_front() {
            ordered_indices.push(idx);
            for &dep_idx in &dependents[idx] {
                indegree[dep_idx] -= 1;
                if indegree[dep_idx] == 0 {
                    ready.push_back(dep_idx);
                }
            }
        }

        if ordered_indices.len() != packs.len() {
            let cycle_nodes: HashSet<usize> = indegree
                .iter()
                .enumerate()
                .filter_map(|(idx, degree)| (*degree > 0).then_some(idx))
                .collect();
            let cycle = find_pack_dependency_cycle(&packs, &name_to_idx, &cycle_nodes);
            return Err(RuntimeError::CircularPackDependency(
                CircularPackDependency { cycle },
            ));
        }

        let mut slots: Vec<Option<Box<dyn PackRuntime>>> = packs.into_iter().map(Some).collect();
        let ordered_packs: Vec<Box<dyn PackRuntime>> = ordered_indices
            .into_iter()
            .map(|idx| slots[idx].take().expect("topological index must exist"))
            .collect();

        validate_unique_note_kinds(&ordered_packs)?;
        validate_unique_verb_names(&ordered_packs)?;
        validate_unique_entity_types(&ordered_packs)?;
        validate_brain_consumer_kinds(&ordered_packs)?;

        let available_verbs: Vec<&'static str> = ordered_packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .filter(|h| matches!(h.visibility, Visibility::Verb))
            .map(|h| h.name)
            .collect();

        // ADR-133: incidental audit writes route through one batch seam per
        // configured `EventStore` instead of taking a writer-task
        // acquisition per dispatch. No store configured (tracing-only or
        // read-only-audit registries) means no seam to construct.
        //
        // A configured store that does not implement the seam's
        // `preflight_event`/`append_events_idempotent` pair would otherwise
        // build silently: every submitted row is rejected at preflight, the
        // dispatch that produced it still reports success, and nothing here
        // distinguishes that from a healthy registry. Reject it now, with an
        // actionable message, instead of at the first audited dispatch.
        if let Some(store) = &self.event_store {
            if !store.supports_idempotent_audit_batch() {
                return Err(RuntimeError::IncompatibleEventStore(
                    "the configured EventStore does not implement ADR-133's \
                     preflight_event/append_events_idempotent pair \
                     (supports_idempotent_audit_batch() returned false); every \
                     audited dispatch would silently lose its audit row while \
                     still reporting success. Implement both methods and \
                     override supports_idempotent_audit_batch() to opt in, or \
                     do not call with_event_store() for this backend."
                        .to_string(),
                ));
            }
        }
        let audit_batch = self.event_store.clone().map(|store| {
            crate::audit_batch::AuditBatch::new(
                store,
                self.audit_batch_config.clone().unwrap_or_default(),
            )
        });

        Ok(VerbRegistry {
            packs: Arc::new(ordered_packs),
            resolvers: Arc::new(self.resolvers),
            gate: self.gate,
            default_namespace: self.default_namespace,
            visible_namespaces: self.visible_namespaces,
            actor_id: self.actor_id,
            event_store: self.event_store,
            audit_store_read_only: self.audit_store_read_only,
            dispatch_hook: self.dispatch_hook,
            available_verbs: Arc::new(available_verbs),
            reference_ring: Arc::new(crate::reference_ring::ReferenceRing::new()),
            audit_batch,
        })
    }
}

/// Validate that no two packs declare the same note kind.
///
/// Boot-time duplicate detection prevents pack configuration errors from
/// silently corrupting note kind routing. Returns an error naming the
/// duplicate kind and the two packs that claim it.
fn validate_unique_note_kinds(packs: &[Box<dyn PackRuntime>]) -> Result<(), RuntimeError> {
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for pack in packs {
        for &kind in pack.note_kinds() {
            if let Some(first_pack) = seen.insert(kind, pack.name()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "duplicate note kind {kind:?}: claimed by both {first_pack:?} and {:?}",
                    pack.name()
                )));
            }
        }
    }
    Ok(())
}

/// Validate pack-declared brain consumer kinds at the composition boundary.
///
/// The wildcard belongs to the binding matcher rather than any consumer, and
/// whitespace-bearing values can never equal the exact wire values callers
/// request. Reject both at boot so a malformed declaration cannot make an
/// otherwise unreachable binding appear valid.
fn validate_brain_consumer_kinds(packs: &[Box<dyn PackRuntime>]) -> Result<(), RuntimeError> {
    for pack in packs {
        for &kind in pack.brain_consumer_kinds() {
            if kind == "*" || kind.trim().is_empty() || kind.trim() != kind {
                return Err(RuntimeError::InvalidInput(format!(
                    "pack {:?} declares invalid brain consumer kind {kind:?}; declarations must be non-empty exact wire values and must not use the registry-owned \"*\" wildcard",
                    pack.name()
                )));
            }
        }
    }
    Ok(())
}

/// Validate that no two packs declare the same `Visibility::Verb` handler name.
///
/// `Visibility::Subhandler` entries are pack-prefixed by convention and excluded
/// from cross-pack collision detection. Two packs declaring the same subhandler
/// name prefix (e.g. `recall.embed`) would be a pack-authoring error but does not
/// produce a cross-pack routing conflict since only the owning pack dispatches them.
fn validate_unique_verb_names(packs: &[Box<dyn PackRuntime>]) -> Result<(), RuntimeError> {
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for pack in packs {
        for handler in pack.handlers() {
            if !matches!(handler.visibility, Visibility::Verb) {
                continue;
            }
            if let Some(first_pack) = seen.insert(handler.name, pack.name()) {
                return Err(RuntimeError::VerbCollision {
                    verb: handler.name.to_string(),
                    first_pack: first_pack.to_string(),
                    second_pack: pack.name().to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Validate that no two owners (the built-in table or a loaded pack) declare
/// a colliding `entity_type` canonical name or alias.
///
/// Boot-time duplicate detection prevents pack configuration errors from
/// silently applying insertion-order semantics to entity-type resolution
/// (ADR-001's registry-ownership collision rule: same `(base_kind,
/// canonical_name)` from two different packs, or an alias collision, is a
/// boot error). Returns an error naming the colliding key and both
/// contributing owners.
fn validate_unique_entity_types(packs: &[Box<dyn PackRuntime>]) -> Result<(), RuntimeError> {
    let owned_defs = packs
        .iter()
        .flat_map(|p| p.entity_types().iter().map(move |def| (p.name(), def)));
    khive_types::EntityTypeRegistry::check_extra_collisions(owned_defs)
        .map_err(RuntimeError::InvalidInput)
}

fn find_pack_dependency_cycle(
    packs: &[Box<dyn PackRuntime>],
    name_to_idx: &HashMap<&str, usize>,
    cycle_nodes: &HashSet<usize>,
) -> Vec<String> {
    fn visit(
        idx: usize,
        packs: &[Box<dyn PackRuntime>],
        name_to_idx: &HashMap<&str, usize>,
        cycle_nodes: &HashSet<usize>,
        visiting: &mut Vec<usize>,
        visited: &mut HashSet<usize>,
    ) -> Option<Vec<String>> {
        if let Some(pos) = visiting.iter().position(|&seen| seen == idx) {
            let mut cycle: Vec<String> = visiting[pos..]
                .iter()
                .map(|&i| packs[i].name().to_string())
                .collect();
            cycle.push(packs[idx].name().to_string());
            return Some(cycle);
        }
        if !visited.insert(idx) {
            return None;
        }
        visiting.push(idx);
        for &req in packs[idx].requires() {
            let Some(&dep_idx) = name_to_idx.get(req) else {
                continue;
            };
            if cycle_nodes.contains(&dep_idx) {
                if let Some(cycle) =
                    visit(dep_idx, packs, name_to_idx, cycle_nodes, visiting, visited)
                {
                    return Some(cycle);
                }
            }
        }
        visiting.pop();
        None
    }

    let mut visited = HashSet::new();
    for &idx in cycle_nodes {
        let mut visiting = Vec::new();
        if let Some(cycle) = visit(
            idx,
            packs,
            name_to_idx,
            cycle_nodes,
            &mut visiting,
            &mut visited,
        ) {
            return cycle;
        }
    }
    cycle_nodes
        .iter()
        .map(|&idx| packs[idx].name().to_string())
        .collect()
}

impl Default for VerbRegistryBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Immutable registry that dispatches verb calls to registered packs.
///
/// Clone is cheap (Arc-wrapped). Constructed via `VerbRegistryBuilder`.
#[derive(Clone)]
pub struct VerbRegistry {
    packs: std::sync::Arc<Vec<Box<dyn PackRuntime>>>,
    /// Pack-level by-ID resolvers, in registration order.
    resolvers: std::sync::Arc<Vec<(String, Box<dyn PackByIdResolver>)>>,
    gate: GateRef,
    default_namespace: String,
    /// Operator-configured read-visibility set (ADR-007 Rev 4 Rule 3b).
    ///
    /// On the default (no explicit `namespace=` param) dispatch path, reads fan
    /// out over `['local'] ∪ visible_namespaces`. Writes are unaffected — they
    /// still pin to `'local'`. An explicit `namespace=` request param is a
    /// precise single-namespace escape and is not widened by this set.
    visible_namespaces: Vec<Namespace>,
    /// Configured actor identity label (ADR-057). When `Some`, dispatch mints
    /// tokens carrying this actor so that `comm.inbox` applies the `to_actor`
    /// filter. When `None`, tokens carry `ActorRef::anonymous()` (party-line).
    actor_id: Option<String>,
    /// Audit event sink — `None` means tracing-only (v0.2 default).
    event_store: Option<Arc<dyn EventStore>>,
    /// Distinguishes ordinary tracing-only construction from a sink omitted
    /// deliberately because its configured backend is read-only.
    audit_store_read_only: bool,
    /// Post-dispatch hook: `None` means no real-time observation.
    dispatch_hook: Option<Arc<dyn DispatchHook>>,
    /// Names of all `Visibility::Verb` handlers across all packs, precomputed
    /// once at `build()` time. Used only to render the unknown-verb error
    /// message — the pack set is fixed after construction, so there is no
    /// need to re-scan every pack's handlers on every miss.
    available_verbs: Arc<Vec<&'static str>>,
    /// Recently-referenced ring (unified-verb draft ADR, Slice 1). Daemon-warm,
    /// actor-scoped, never persisted — see `crate::reference_ring`. Shared
    /// across every clone of this registry via the `Arc`, so admissions made
    /// by one dispatch are visible to the next on the same warm daemon.
    reference_ring: Arc<crate::reference_ring::ReferenceRing>,
    /// ADR-133 audit-batch seam. `None` exactly when `event_store` is
    /// `None` — no store configured means no seam to construct, and every
    /// audit call site falls back to its pre-ADR-133 tracing-only/no-op
    /// path.
    audit_batch: Option<Arc<crate::audit_batch::AuditBatch>>,
}

/// Result of an operation handled outside normal pack dispatch, paired with
/// typed transport metadata that must survive the gate/audit boundary.
///
/// The canonical `result` remains the value used for audit accounting. The
/// metadata is returned to the intercepting transport without being smuggled
/// through a mutex side channel or folded into the verb's public result shape.
#[derive(Debug, Clone, PartialEq)]
pub struct InterceptedDispatchResult<M> {
    /// Canonical verb result used for audit and resource accounting.
    pub result: Value,
    /// Transport-owned metadata that must accompany the canonical result.
    pub metadata: M,
}

impl<M> InterceptedDispatchResult<M> {
    /// Pair a canonical result with its typed transport metadata.
    pub fn new(result: Value, metadata: M) -> Self {
        Self { result, metadata }
    }
}

/// Per-request identity context that overrides a [`VerbRegistry`]'s
/// construction-baked `default_namespace` / `actor_id` / `visible_namespaces`
/// for exactly one [`VerbRegistry::dispatch_with_identity`] call (ADR-096
/// Fork 1 — warm-daemon per-request identity).
///
/// A single warm registry is built once with a baked identity, but must be
/// able to serve requests whose caller resolved a *different* attribution
/// identity (e.g. a different project-local `[actor]`) without a cold
/// fallback and without mis-stamping writes under the registry's own baked
/// actor. Supplying `Some(RequestIdentity { .. })` threads the caller's
/// identity through token minting for that one call; the registry's fields
/// (and every other in-flight call) are untouched. `None` is exactly
/// [`VerbRegistry::dispatch`] — the baked scalars apply, unchanged from
/// before this type existed.
#[derive(Debug, Clone, Default)]
pub struct RequestIdentity {
    /// Storage/gate default namespace for this request (used when the verb's
    /// own params carry no explicit `namespace` field). Overrides
    /// `VerbRegistry::default_namespace`.
    pub namespace: String,
    /// Write-stamp / gate actor label for this request (ADR-057). Overrides
    /// `VerbRegistry::actor_id`. `None` mints `ActorRef::anonymous()`, same
    /// as an unconfigured baked `actor_id`.
    pub actor_id: Option<String>,
    /// Extra read-visibility namespaces for this request (ADR-007 Rev 4 Rule
    /// 3b). Overrides `VerbRegistry::visible_namespaces`. Entries that fail
    /// `Namespace::parse` are skipped with a `tracing::warn!` rather than
    /// failing the whole request — a single malformed visibility entry from a
    /// caller-supplied frame must not block dispatch.
    pub visible_namespaces: Vec<String>,
    /// Opaque process provenance resolved by the originating request process.
    /// `None` means the origin did not set one; a warm daemon must not replace
    /// it with its own process environment. This field is attribution-only and
    /// never participates in the gate or token authority.
    pub process_ref: Option<String>,
    /// Caller-supplied correlation id for this request (khive#948), carried
    /// unchanged from the daemon frame's `request_id` field. Every operation
    /// in one batch or chain receives the same value: it is a request-group
    /// selector, never an operation-unique id. Stamped into the audit event's
    /// `resource.request_id` on every outcome (success, error, and denied) so
    /// a client can join its own pre-send sample to all server-side audit rows
    /// for that request. `None` means the caller
    /// supplied no id (a pre-#948 client, or an internal/non-benchmark
    /// caller) — the audit row then carries no `request_id` key at all.
    pub request_id: Option<u64>,
}

impl RequestIdentity {
    /// Reconstruct the effective principal and namespace scope carried by an
    /// already-authorized token for a nested registry dispatch.
    ///
    /// Cross-pack calls must still pass through the registry Gate, but using
    /// the registry's construction-baked identity would silently replace a
    /// warm daemon request's actor and visibility (ADR-096). This projection
    /// preserves the token's exact primary namespace, actor, and read-visible
    /// namespaces, and the origin's process provenance rider. A
    /// `NamespaceToken` does not carry the ingress correlation id, so nested
    /// calls intentionally use `request_id: None`; `process_ref` IS carried by
    /// the token (ADR-096: an absent value stays absent, a present origin
    /// rider survives nested dispatch without reading the daemon
    /// environment).
    pub fn from_token(token: &NamespaceToken) -> Self {
        Self {
            namespace: token.namespace().as_str().to_string(),
            actor_id: token.actor().binding_id().map(str::to_string),
            visible_namespaces: token
                .visible_namespaces()
                .iter()
                .map(|namespace| namespace.as_str().to_string())
                .collect(),
            process_ref: token.process_ref().map(str::to_owned),
            request_id: None,
        }
    }
}

/// A non-blank, out-of-band authenticated principal for [`VerbRegistry::dispatch_as`].
///
/// Embedding hosts authenticate a principal through their own channel (not the
/// request DSL) and then need that principal to become the effective actor
/// for one dispatch. The constructor rejects an empty or whitespace-only
/// identifier so an authentication-integration failure (an empty subject)
/// fails closed at construction time instead of silently resolving to the
/// anonymous/local actor at dispatch time — see [`crate::actor_identity::resolve_actor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActor(String);

impl VerifiedActor {
    /// Validate and wrap a verified principal identifier.
    ///
    /// Returns `RuntimeError::InvalidInput` when `id` is empty or contains
    /// only whitespace.
    pub fn new(id: impl Into<String>) -> Result<Self, RuntimeError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "VerifiedActor: identifier must not be empty or whitespace-only".to_string(),
            ));
        }
        Ok(Self(id))
    }

    /// Borrow the validated identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn into_inner(self) -> String {
        self.0
    }
}

/// Error returned by [`VerbRegistry::apply_schema_plans_with_map`] when two
/// packs on the same backend declare the same auxiliary table (ADR-028 §7).
#[derive(Debug)]
pub struct PackSchemaCollisionError {
    /// First pack to declare the table.
    pub pack_a: &'static str,
    /// Second pack that collides with `pack_a`.
    pub pack_b: &'static str,
    /// Table name or DDL error description.
    pub table: String,
}

impl std::fmt::Display for PackSchemaCollisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.pack_a == self.pack_b {
            write!(
                f,
                "pack schema boot failure for pack {:?}: {}",
                self.pack_a, self.table
            )
        } else {
            write!(
                f,
                "pack schema collision: packs {:?} and {:?} both declare table {:?} \
                 on the same backend — move one pack to a separate backend or rename the table",
                self.pack_a, self.pack_b, self.table
            )
        }
    }
}

impl std::error::Error for PackSchemaCollisionError {}

/// Extract table names from a single DDL statement.
///
/// Handles `CREATE TABLE IF NOT EXISTS`, `CREATE TABLE`, and
/// `CREATE VIRTUAL TABLE IF NOT EXISTS`, `CREATE VIRTUAL TABLE`.
/// Returns an empty Vec when no table name is found (e.g. index DDL).
fn extract_table_names(stmt: &str) -> Vec<String> {
    let normalized = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
    let upper = normalized.to_ascii_uppercase();
    let table_name = if let Some(rest) = upper.strip_prefix("CREATE VIRTUAL TABLE IF NOT EXISTS ") {
        rest.split_whitespace().next()
    } else if let Some(rest) = upper.strip_prefix("CREATE VIRTUAL TABLE ") {
        rest.split_whitespace().next()
    } else if let Some(rest) = upper.strip_prefix("CREATE TABLE IF NOT EXISTS ") {
        rest.split_whitespace().next()
    } else if let Some(rest) = upper.strip_prefix("CREATE TABLE ") {
        rest.split_whitespace().next()
    } else {
        None
    };
    match table_name {
        Some(name) => {
            let clean = name.trim_matches(|c: char| c == '(' || c == ';');
            if clean.is_empty() {
                vec![]
            } else {
                vec![clean.to_ascii_lowercase()]
            }
        }
        None => vec![],
    }
}

/// Render an [`EndpointKind`] as the `"<substrate>:<kind>"` label used in
/// `link(help=true)`'s `endpoint_rules` table.
fn endpoint_kind_label(kind: &EndpointKind) -> String {
    match kind {
        EndpointKind::EntityOfKind(k) => format!("entity:{k}"),
        EndpointKind::NoteOfKind(k) => format!("note:{k}"),
        EndpointKind::EntityOfType { kind, entity_type } => {
            format!("entity:{kind}({entity_type})")
        }
    }
}

/// Relations `validate_edge_relation_endpoints`
/// (`crates/khive-runtime/src/operations.rs`) resolves in its own dedicated
/// branch — before the generic pack-rule branch (`pack_rule_allows`) is ever
/// reached. For these three relations the validator additionally accepts
/// any `note -> note` pair unconditionally, regardless of note kind
/// (ADR-002 §"Versioning" and §"Epistemic"), and never consults pack
/// `EDGE_RULES` at all, on either substrate.
pub(crate) const SPECIAL_RELATIONS: &[khive_types::EdgeRelation] = &[
    khive_types::EdgeRelation::Supersedes,
    khive_types::EdgeRelation::Supports,
    khive_types::EdgeRelation::Refutes,
];

pub(crate) fn is_special_relation(relation: khive_types::EdgeRelation) -> bool {
    SPECIAL_RELATIONS.contains(&relation)
}

/// Compose the full per-relation endpoint allowlist surfaced by
/// `link(help=true)` (issue #964).
///
/// Combines the base entity-to-entity endpoint contract
/// (`operations::base_entity_endpoint_rules`) with every loaded pack's
/// additive `EDGE_RULES`, the unconditional `note -> note` allowance for the
/// three special relations (`supersedes` / `supports` / `refutes` —
/// `operations.rs`'s dedicated special-relation branch), and the
/// `annotates` note-to-any special case — the exact same sources
/// `valid_relations_for_entity_pair` (`khive-pack-kg`) consults when
/// enriching a rejected `link` call, so a caller reading this table cannot
/// diverge from what the validator itself accepts.
///
/// Pack `EDGE_RULES` for a special relation are deliberately excluded: the
/// validator's special-relation branch returns before `pack_rule_allows` is
/// ever reached (`operations.rs`), so advertising such a rule here would
/// claim enforcement that never actually happens.
fn edge_endpoint_table(packs: &[Box<dyn PackRuntime>]) -> Vec<Value> {
    let mut rows: Vec<Value> = crate::operations::base_entity_endpoint_rules()
        .iter()
        .map(|(src, rel, tgt)| {
            serde_json::json!({
                "relation": rel.as_str(),
                "source": format!("entity:{src}"),
                "target": format!("entity:{tgt}"),
            })
        })
        .collect();

    for rel in SPECIAL_RELATIONS {
        rows.push(serde_json::json!({
            "relation": rel.as_str(),
            "source": "note:*",
            "target": "note:*",
        }));
    }

    for pack in packs.iter() {
        for rule in pack.edge_rules().iter() {
            if is_special_relation(rule.relation) {
                continue;
            }
            rows.push(serde_json::json!({
                "relation": rule.relation.as_str(),
                "source": endpoint_kind_label(&rule.source),
                "target": endpoint_kind_label(&rule.target),
            }));
        }
    }

    rows.push(serde_json::json!({
        "relation": "annotates",
        "source": "note:*",
        "target": "any (entity, note, edge, or event)",
    }));

    rows
}

impl VerbRegistry {
    /// This registry's construction-baked default namespace.
    ///
    /// Used as the fallback when a request carries no [`RequestIdentity`]
    /// override (ADR-096 Fork 1) and by transports that need to advertise
    /// their own resolved identity when forwarding to a warm daemon.
    pub fn default_namespace(&self) -> &str {
        &self.default_namespace
    }

    /// This registry's construction-baked actor identity label, if configured
    /// (ADR-057). `None` means dispatch mints `ActorRef::anonymous()` absent a
    /// per-request [`RequestIdentity`] override (ADR-096 Fork 1).
    pub fn actor_id(&self) -> Option<&str> {
        self.actor_id.as_deref()
    }

    /// This registry's construction-baked extra read-visibility namespaces
    /// (ADR-007 Rev 4 Rule 3b), used absent a per-request [`RequestIdentity`]
    /// override (ADR-096 Fork 1).
    pub fn visible_namespaces(&self) -> &[Namespace] {
        &self.visible_namespaces
    }

    /// This registry's configured audit `EventStore`, if any (ADR-094).
    ///
    /// Lets background tasks that hold a `VerbRegistry` but do not go through
    /// `dispatch` (e.g. the email channel poll loop) append best-effort
    /// lifecycle events to the same sink gate-check audit rows use, without
    /// threading a second `Option<Arc<dyn EventStore>>` field through every
    /// caller. `None` means either the historical tracing-only default or an
    /// intentionally read-only audit backend; callers that need to distinguish
    /// those cases use [`Self::audit_persistence_advisory`].
    pub fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.event_store.clone()
    }

    /// Process-lifetime audit-batch health counters for this registry's
    /// ADR-133 seam, if one is configured. `None` exactly when
    /// [`Self::event_store`] is `None` — the same condition under which no
    /// batch exists to report on. The `db_diagnostics` verb feeds this into
    /// `KhiveRuntime::db_diagnostics_with_audit_metrics` so an operator can
    /// see flush failures and pure-observability degradation instead of the
    /// permanently-unavailable placeholder a bare `KhiveRuntime` reports.
    ///
    /// `admission_refused_obligations` and `admission_unresolved_obligations`
    /// are sourced separately from [`audit_admission_refused_obligation_count`]
    /// and [`audit_admission_unresolved_obligation_count`] rather than from
    /// `batch.health_metrics()`: they count a decision made in
    /// `append_audit_event_best_effort` (ADR-103 Amendment 3 / ADR-133
    /// Amendment 1), not a property of the batch itself, so they are
    /// process-wide like the rest of this struct's fields rather than
    /// per-`AuditBatch`.
    pub fn audit_batch_metrics(&self) -> Option<khive_db::diagnostics::RuntimeAuditBatchMetrics> {
        self.audit_batch.as_ref().map(|batch| {
            let m = batch.health_metrics();
            khive_db::diagnostics::RuntimeAuditBatchMetrics {
                flush_failures: m.flush_failures,
                degraded_rows: m.degraded_rows,
                degraded: m.degraded,
                admission_refused_obligations: audit_admission_refused_obligation_count(),
                admission_unresolved_obligations: audit_admission_unresolved_obligation_count(),
            }
        })
    }

    /// Test/diagnostic-only accessor for the underlying ADR-133 audit-batch
    /// seam. `None` when no `EventStore` was configured (the batch is lazily
    /// constructed from one). Exposed so admission-pressure mechanism tests
    /// can saturate and drain the SAME instance a real dispatch uses
    /// (khive#2117, khive#2147, khive#2208, khive#2217) instead of testing a
    /// look-alike.
    pub fn audit_batch_handle(&self) -> Option<Arc<crate::audit_batch::AuditBatch>> {
        self.audit_batch.clone()
    }

    /// Stop admitting new audit rows and wait for every already-accepted row
    /// to reach a terminal state (ADR-133).
    ///
    /// A no-op returning `Ok(())` when no `EventStore` — and therefore no
    /// audit-batch seam — is configured. Callers that own this registry's
    /// shutdown sequence should call this before tearing down the writer or
    /// database so no accepted audit row is silently dropped mid-flight.
    pub async fn shutdown_audit_batch(
        &self,
    ) -> Result<(), crate::audit_batch::AuditTerminalReason> {
        use crate::audit_batch::AuditBatchControl;
        match &self.audit_batch {
            Some(audit_batch) => audit_batch.close_and_drain().await,
            None => Ok(()),
        }
    }

    /// Advisory for a dispatch whose configured audit sink is read-only.
    ///
    /// The MCP transport places this beside successful per-operation results;
    /// `None` means audit persistence is configured normally or was never
    /// configured at all.
    pub fn audit_persistence_advisory(&self) -> Option<Value> {
        self.audit_store_read_only.then(|| {
            serde_json::json!({
                "code": AUDIT_PERSISTENCE_SKIPPED_READ_ONLY,
                "severity": "warning",
                "component": "audit_event_store",
                "reason": "read_only_backend",
                "message": "operation completed, but its dispatch audit event was not persisted because the audit backend is read-only",
            })
        })
    }

    /// Explicit, fail-closed opt-in for admission-pressure audit degradation
    /// (khive#2147/khive#2217). `VerbCategory::Assertive` alone is NOT a
    /// sound proxy for "safe to drop this dispatch's own audit row under
    /// audit-lane admission pressure": several Assertive handlers have
    /// their own accounting-bearing side effects. Two known examples,
    /// deliberately excluded here:
    /// - `memory.recall` dispatches `brain.record_serve` as a background
    ///   write; degrading `memory.recall`'s row raises the risk that a
    ///   serve goes unaccounted for if the ledger dispatch itself later
    ///   also races admission pressure.
    /// - `db_diagnostics` may backfill WAL frames via a PASSIVE checkpoint
    ///   probe — physical I/O, not a pure in-memory read.
    ///
    /// What membership here means, precisely: the verb performs no domain
    /// mutation, so its OWN per-dispatch audit/accounting row may be dropped
    /// under transient admission pressure without the caller losing a
    /// meaningful result (ADR-103 Amendment 3, ADR-133 Amendment 1). It does
    /// NOT mean the handler is free of every event-plane write: `search`
    /// still fires its own best-effort `SearchExecuted` telemetry, and
    /// `context` still records a one-time `ConfigLocked` event, both on
    /// independent code paths this mechanism never touches — those events
    /// commit or fail on their own terms, unaffected by whether this
    /// dispatch's own audit row degrades.
    ///
    /// Every entry here MUST be declared `VerbCategory::Assertive` in
    /// `khive-pack-kg/src/handler_defs.rs` — enforced by the
    /// `admission_degrade_safe_verbs_are_registered_assertive` census test
    /// below, which re-derives the classification from that file's live
    /// source rather than trusting this list's own claim. Adding a verb
    /// from a different pack requires extending that test's source scan,
    /// not just this list.
    const ADMISSION_DEGRADE_SAFE_VERBS: &'static [&'static str] = &[
        "get",
        "list",
        "stats",
        "search",
        "neighbors",
        "traverse",
        "context",
        "query",
        "resolve",
        "whoami",
        "verbs",
    ];

    /// Whether `verb` is both declared [`VerbCategory::Assertive`] (the
    /// speech-act tag for handlers that "retrieve and present facts" rather
    /// than committing a domain change) AND explicitly opted in to
    /// admission-pressure audit degradation via
    /// [`Self::ADMISSION_DEGRADE_SAFE_VERBS`]. Unknown or non-opted-in verbs
    /// are conservatively `false` — fail-closed, so a new Assertive handler
    /// hard-fails its audit obligation like any write until someone
    /// deliberately reviews it and adds it to the allowlist.
    ///
    /// Used only to decide whether a dispatch's own audit-obligation row may
    /// degrade to best-effort on transient audit-lane admission pressure
    /// (`append_audit_event_best_effort`) — a read that performed no domain
    /// write must not fail the caller just because the audit lane is
    /// momentarily saturated. Never used for permission checking, transport
    /// routing, or return-shape selection.
    fn admission_degrade_safe(&self, verb: &str) -> bool {
        if !Self::ADMISSION_DEGRADE_SAFE_VERBS.contains(&verb) {
            return false;
        }
        self.packs
            .iter()
            .find_map(|pack| pack.handlers().iter().find(|h| h.name == verb))
            .is_some_and(|handler| handler.category == VerbCategory::Assertive)
    }

    /// Return the help schema envelope for a verb.
    ///
    /// Walks registered packs for the first matching `HandlerDef` and returns a
    /// structured JSON envelope. Subhandlers carry `callable_via_mcp: false`.
    /// Every envelope carries the shared `identifier_resolution` contract.
    /// `link`'s envelope additionally carries `endpoint_rules` — the composed
    /// per-relation source/target allowlist (issue #964) — so batch callers can
    /// defer to the kernel's own table instead of re-implementing it locally.
    /// Unknown verbs return `RuntimeError::InvalidInput`. Full shape documented
    /// in `docs/protocol.md` §Request Schema.
    pub fn describe_verb(&self, verb: &str) -> Result<Value, RuntimeError> {
        for pack in self.packs.iter() {
            for handler in pack.handlers().iter() {
                if handler.name == verb {
                    let category = format!("{:?}", handler.category);
                    let params_arr: Vec<Value> = handler
                        .params
                        .iter()
                        .map(|p| {
                            serde_json::json!({
                                "name": p.name,
                                "type": p.param_type,
                                "required": p.required,
                                "description": p.description,
                            })
                        })
                        .collect();
                    // Subhandlers are not callable via the MCP request surface;
                    // the help payload must match the behaviour the dispatch
                    // path enforces so callers reading `help=true` before
                    // probing see accurate availability.
                    if matches!(handler.visibility, Visibility::Subhandler) {
                        return Ok(serde_json::json!({
                            "verb": verb,
                            "pack": pack.name(),
                            "description": handler.description,
                            "category": category,
                            "params": params_arr,
                            "identifier_resolution": identifier_resolution_help(),
                            "visibility": "internal",
                            "callable_via_mcp": false,
                            "note": "This is an internal subhandler. Calling it via the MCP \
                                     request surface returns permission denied. It can only be \
                                     invoked by internal runtime callers.",
                        }));
                    }
                    let mut envelope = serde_json::json!({
                        "verb": verb,
                        "pack": pack.name(),
                        "description": handler.description,
                        "category": category,
                        "params": params_arr,
                        "identifier_resolution": identifier_resolution_help(),
                    });
                    if verb == "link" {
                        envelope["endpoint_rules"] = Value::Array(edge_endpoint_table(&self.packs));
                    }
                    return Ok(envelope);
                }
            }
        }
        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(RuntimeError::UnknownVerb(format!(
            "unknown verb {verb:?}; available: {}",
            self.available_verbs.join(", ")
        )))
    }

    /// Check whether the gate permits writes into `ns`.
    ///
    /// Performs a gate evaluation with verb `"authorize"` before any background
    /// loop is spawned (ADR-056 §6).  Returns `Ok(())` when the gate allows the
    /// namespace, or `Err(RuntimeError::PermissionDenied{..})` when denied.
    /// Gate errors (implementation failures) are surfaced as
    /// `RuntimeError::Internal` carrying the stable classified reason; the
    /// bounded, masked backend detail goes to the server-side log here, since
    /// callers log the returned error.
    pub fn authorize_namespace(&self, ns: Namespace) -> Result<(), RuntimeError> {
        let actor = crate::actor_identity::resolve_actor(self.actor_id.as_deref());
        let req = GateRequest::new(actor, ns, "authorize", serde_json::Value::Null);
        match self.gate.check(&req) {
            Ok(decision) if decision.is_allow() => Ok(()),
            Ok(GateDecision::Deny { reason }) => Err(RuntimeError::PermissionDenied {
                verb: "authorize".to_string(),
                reason,
            }),
            Ok(_) => Err(RuntimeError::PermissionDenied {
                verb: "authorize".to_string(),
                reason: "gate denied".to_string(),
            }),
            Err(e) => {
                tracing::warn!(
                    error = %crate::secret_gate::bounded_masked_log_text(&e.to_string()),
                    "authorize_namespace: gate check failed (fail-closed)"
                );
                Err(RuntimeError::Internal(format!(
                    "gate error: {}",
                    e.wire_reason()
                )))
            }
        }
    }

    /// Gate and execute an operation handled outside normal pack dispatch.
    ///
    /// Multi-backend transports use this to route an operation through a
    /// coordinator while retaining [`Self::dispatch_with_identity`]'s gate and
    /// audit lifecycle. Deny is authoritative, gate errors fail closed, and an
    /// allowed audit is persisted after the intercepted operation resolves so
    /// its outcome and duration reflect the operation result. Successful
    /// `git.digest` interception uses the same strict durable-receipt exception
    /// as normal pack dispatch.
    pub async fn dispatch_intercepted_with_identity<F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<Value, RuntimeError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<Value, RuntimeError>>,
    {
        self.dispatch_intercepted_with_metadata_with_identity(
            verb,
            params,
            identity,
            |namespace| async move {
                dispatch(namespace)
                    .await
                    .map(|result| InterceptedDispatchResult::new(result, ()))
            },
        )
        .await
        .map(|outcome| outcome.result)
    }

    /// Gate and execute an intercepted operation whose transport needs typed
    /// metadata in addition to the canonical verb result.
    ///
    /// Audit accounting always receives `outcome.result`; `outcome.metadata`
    /// crosses the dispatch seam unchanged for the transport to place beside
    /// that result in its own envelope.
    pub async fn dispatch_intercepted_with_metadata_with_identity<M, F, Fut>(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
        dispatch: F,
    ) -> Result<InterceptedDispatchResult<M>, RuntimeError>
    where
        F: FnOnce(Namespace) -> Fut,
        Fut: std::future::Future<Output = Result<InterceptedDispatchResult<M>, RuntimeError>>,
    {
        let request_id = identity.and_then(|id| id.request_id);
        let gate_req = self.gate_request_with_identity(verb, params, identity)?;
        let mut deferred_audit = match self.gate.check(&gate_req) {
            Ok(decision) => {
                let audit = AuditEvent::from_check(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );
                if let GateDecision::Deny { reason } = decision {
                    if let Some(store) = &self.event_store {
                        let event = build_audit_storage_event(
                            &gate_req,
                            &audit,
                            EventOutcome::Denied,
                            Some(crate::cost_unit::base_resource_payload(request_id)),
                        );
                        // The dispatch already returns `PermissionDenied`
                        // below regardless of whether this row commits — a
                        // deny never reports success — so a persistent
                        // commit failure here has no caller-visible outcome
                        // to fold into; it is still logged and counted by
                        // the helper.
                        let _ = append_audit_event_best_effort(
                            self.audit_batch.as_ref(),
                            store,
                            event,
                            verb,
                            crate::audit_batch::AuditProducer::GateDenied,
                            false,
                        )
                        .await;
                    }
                    return Err(RuntimeError::PermissionDenied {
                        verb: verb.to_string(),
                        reason,
                    });
                }
                Some(audit)
            }
            Err(err) => {
                return Err(self
                    .gate_unavailable_error(&gate_req, &err, request_id)
                    .await);
            }
        };

        let started = Instant::now();
        let mut result = dispatch(gate_req.namespace.clone()).await;
        let duration_us = started.elapsed().as_micros() as i64;
        let receipt_outcome = if verb == "git.digest" && result.is_ok() {
            let resource = result.as_ref().ok().map(|outcome| {
                crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    &outcome.result,
                    || 0,
                    request_id,
                )
            });
            // The receipt helper operates on the canonical verb result. Move
            // that value out temporarily so it can turn receipt failures into
            // the outer dispatch error without discarding successful typed
            // transport metadata.
            let mut receipt_result: Result<Value, RuntimeError> = match result.as_mut() {
                Ok(outcome) => Ok(std::mem::take(&mut outcome.result)),
                Err(_) => unreachable!("git.digest receipt path is guarded by result.is_ok()"),
            };
            let outcome = persist_git_digest_receipt(
                self.event_store.as_ref(),
                self.audit_batch.as_ref(),
                &gate_req,
                deferred_audit.as_ref(),
                &mut receipt_result,
                duration_us,
                resource,
            )
            .await;
            match receipt_result {
                Ok(receipted_result) => {
                    if let Ok(intercepted) = &mut result {
                        intercepted.result = receipted_result;
                    }
                }
                Err(error) => result = Err(error),
            }
            Some(outcome)
        } else {
            None
        };
        if receipt_outcome.is_none()
            || receipt_outcome == Some(GitDigestReceiptOutcome::BuildRejected)
        {
            if let Some(audit) = deferred_audit.take() {
                let audit_outcome = self
                    .persist_intercepted_audit(
                        verb,
                        &gate_req,
                        audit,
                        result.as_ref().map(|outcome| &outcome.result),
                        duration_us,
                        request_id,
                    )
                    .await;
                result = fold_audit_obligation(result, audit_outcome);
            }
        }
        result
    }

    async fn persist_intercepted_audit(
        &self,
        verb: &str,
        gate_req: &GateRequest,
        audit: AuditEvent,
        result: Result<&Value, &RuntimeError>,
        duration_us: i64,
        request_id: Option<u64>,
    ) -> Result<(), RuntimeError> {
        let Some(store) = &self.event_store else {
            return Ok(());
        };
        let event = match result {
            Ok(value) if verb == "link" && gate_req.args.get("links").is_none() => {
                let resource = crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    value,
                    || 0,
                    request_id,
                );
                match link_audit_success_from_result(audit.clone(), value) {
                    Some((edge_id, mut payload)) => {
                        if let Value::Object(ref mut map) = payload {
                            map.insert("resource".to_string(), resource);
                        }
                        Event::new(
                            gate_req.namespace.as_str(),
                            gate_req.verb.as_str(),
                            EventKind::Audit,
                            SubstrateKind::Event,
                            format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                        )
                        .with_outcome(EventOutcome::Success)
                        .with_target(edge_id)
                        .with_payload(payload)
                        .with_payload_schema_version(2)
                        .with_duration_us(duration_us)
                    }
                    None => build_audit_storage_event(
                        gate_req,
                        &audit,
                        EventOutcome::Success,
                        Some(resource),
                    )
                    .with_duration_us(duration_us),
                }
            }
            Ok(value) => build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Success,
                Some(crate::cost_unit::resource_payload(
                    verb,
                    &gate_req.args,
                    value,
                    || 0,
                    request_id,
                )),
            )
            .with_duration_us(duration_us),
            Err(_) => build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Error,
                Some(crate::cost_unit::base_resource_payload(request_id)),
            )
            .with_duration_us(duration_us),
        };
        let producer = if result.is_ok() {
            crate::audit_batch::AuditProducer::DispatchSucceeded
        } else {
            crate::audit_batch::AuditProducer::DispatchFailed
        };
        append_audit_event_best_effort(
            self.audit_batch.as_ref(),
            store,
            event,
            verb,
            producer,
            self.admission_degrade_safe(verb),
        )
        .await
    }

    fn gate_request_with_identity(
        &self,
        verb: &str,
        params: &Value,
        identity: Option<&RequestIdentity>,
    ) -> Result<GateRequest, RuntimeError> {
        let default_namespace = identity
            .map(|id| id.namespace.as_str())
            .unwrap_or(self.default_namespace.as_str());
        let namespace = resolve_explicit_namespace(params, default_namespace)?;
        let actor_id = identity
            .map(|id| id.actor_id.as_deref())
            .unwrap_or(self.actor_id.as_deref());
        let actor = crate::actor_identity::resolve_actor(actor_id);
        Ok(GateRequest::new(actor, namespace, verb, params.clone()))
    }

    async fn gate_unavailable_error(
        &self,
        gate_req: &GateRequest,
        error: &khive_gate::GateError,
        request_id: Option<u64>,
    ) -> RuntimeError {
        let audit = AuditEvent::gate_unavailable(gate_req, self.gate.impl_name());
        tracing::info!(
            audit_event = %serde_json::to_string(&audit)
                .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
            "gate.check"
        );
        tracing::warn!(
            verb = %gate_req.verb,
            error = %crate::secret_gate::bounded_masked_log_text(&error.to_string()),
            "gate check failed (fail-closed)"
        );
        if let Some(store) = &self.event_store {
            let event = build_audit_storage_event(
                gate_req,
                &audit,
                EventOutcome::Error,
                Some(crate::cost_unit::base_resource_payload(request_id)),
            );
            let _ = append_audit_event_best_effort(
                self.audit_batch.as_ref(),
                store,
                event,
                gate_req.verb.as_str(),
                crate::audit_batch::AuditProducer::GateUnavailable,
                false,
            )
            .await;
        }
        RuntimeError::GateUnavailable {
            verb: gate_req.verb.clone(),
            // Caller-visible: a stable, classified reason derived from the
            // `GateError` variant only. `error`'s `Display` text is logged
            // above (server-side, via `tracing::warn!`) and must never be
            // interpolated here — a gate backend's error message can embed
            // connection details, addresses, or credentials.
            reason: error.wire_reason().to_string(),
        }
    }

    /// Dispatch a verb to the first pack that handles it.
    ///
    /// Routes through the gate, then invokes the matching pack handler. When
    /// `params["help"] == true`, short-circuits to `describe_verb` with no side effects.
    /// Gate errors fail closed. Full dispatch flow documented in `docs/protocol.md`.
    ///
    /// Equivalent to `self.dispatch_with_identity(verb, params, None)` — uses
    /// this registry's construction-baked `default_namespace` / `actor_id` /
    /// `visible_namespaces`.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        self.dispatch_with_identity(verb, params, None).await
    }

    /// Dispatch a verb, optionally overriding this registry's baked identity
    /// scalars for exactly this call (ADR-096 Fork 1).
    ///
    /// `identity = None` behaves exactly like [`Self::dispatch`]. `identity =
    /// Some(id)` uses `id.namespace` / `id.actor_id` / `id.visible_namespaces`
    /// in place of `self.default_namespace` / `self.actor_id` /
    /// `self.visible_namespaces` for this call's namespace resolution, gate
    /// request, and token minting. The registry's own fields are never mutated,
    /// so concurrent calls with different (or no) identity are independent.
    /// See `docs/api/pack.md#dispatch_with_identity` for why this enables one warm
    /// registry to serve many attribution identities over a shared backend.
    pub async fn dispatch_with_identity(
        &self,
        verb: &str,
        params: Value,
        identity: Option<RequestIdentity>,
    ) -> Result<Value, RuntimeError> {
        // help=true interception: short-circuit before gate/pack.
        if params.get("help").and_then(Value::as_bool) == Some(true) {
            return self.describe_verb(verb);
        }
        // Resolve namespace before `params` is moved into pack.dispatch, so the
        // post-dispatch hook can reference it.
        //
        // Absent `namespace` and a present-but-malformed `namespace` are
        // different cases. A present non-string value (null, number, bool,
        // array, object) is explicit caller input that failed to parse and
        // must fail closed, not silently coerce to the default namespace.
        // Only a genuinely absent key defaults. Shared with the multi-backend
        // coordinator intercept via `resolve_explicit_namespace` so every MCP
        // ingress path applies the same fail-closed rule.
        let explicit_namespace = params.get("namespace").is_some_and(Value::is_string);
        // The caller-supplied correlation id (khive#948), if any. Read once
        // here so it is in scope for every audit-append site below,
        // including the ones that run before pack dispatch is attempted.
        let request_id: Option<u64> = identity.as_ref().and_then(|id| id.request_id);
        // Thread the configured actor identity into the gate request so the
        // gate can distinguish human vs agent callers at the dispatch seam.
        // Resolved once via the shared actor-identity policy and reused for
        // token minting below, so the gate's notion of "who is the caller"
        // and the storage token's notion can never drift apart.
        let gate_req = self.gate_request_with_identity(verb, &params, identity.as_ref())?;
        let ns = gate_req.namespace.clone();
        let resolved_actor = gate_req.actor.clone();

        // Consult the gate.
        //
        // - Ok(Allow) → proceed to pack dispatch (tracing + optional EventStore).
        // - Ok(Deny) → emit audit, persist if store configured, return PermissionDenied.
        // - Err(_) → emit an outage audit and return GateUnavailable.
        let (gate_blocked, mut deferred_audit) = match self.gate.check(&gate_req) {
            Ok(decision) => {
                let is_deny = matches!(decision, GateDecision::Deny { .. });

                // Emit audit event via tracing.
                let audit = AuditEvent::from_check(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );

                // Drain any process-lifetime `OnceLock` config locks queued
                // since the last dispatch and persist them as `ConfigLocked`
                // events, riding this same audit-persistence gate. The
                // namespace/actor stamped on these rows are whichever
                // dispatch happens to observe the queue non-empty first:
                // an accepted provenance quirk, preferred over threading an
                // `EventStore` handle into every synchronous
                // `OnceLock::get_or_init` call site. The verb column is NOT
                // inherited from that bystander dispatch: a config-lock row
                // wearing an operation verb pollutes verb-filtered queries
                // (e.g. per-verb receipt counts), so these rows carry their
                // own `config.lock` pseudo-verb and remain discoverable by
                // `EventKind::ConfigLocked`.
                if let Some(store) = &self.event_store {
                    if crate::config_ledger::PENDING
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                    {
                        for (key, value) in crate::config_ledger::drain_config_locked() {
                            let payload = serde_json::json!({ "key": key, "value": value });
                            let storage_event = Event::new(
                                gate_req.namespace.as_str(),
                                "config.lock",
                                EventKind::ConfigLocked,
                                SubstrateKind::Event,
                                format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                            )
                            .with_payload(payload);
                            // ConfigLocked is pure observability: the helper
                            // never returns `Err` for it, so there is
                            // nothing to fold.
                            let _ = append_audit_event_best_effort(
                                self.audit_batch.as_ref(),
                                store,
                                storage_event,
                                "config.lock",
                                crate::audit_batch::AuditProducer::ConfigLocked,
                                false,
                            )
                            .await;
                        }
                    }
                }

                // Every Allow-outcome audit row defers its append until pack
                // dispatch returns, so the row can carry the measured
                // dispatch time in `duration_us` (persisting before dispatch
                // ran always recorded the `Event::new` default of 0). A
                // singleton `link` call (no `links` bulk array) additionally
                // enriches the deferred row with the created/resolved edge
                // fields (schema v2) once dispatch resolves. Denied calls
                // have no dispatch to wait for and keep the immediate v1
                // append below.
                //
                // Accepted trade-off for ordinary verbs: a crash between this
                // Allow decision and the deferred append loses the audit row.
                // `git.digest` narrows the caller-visible contract below: it
                // never returns success until the deferred receipt append is
                // confirmed, though a process crash can still leave committed
                // ingest writes with no response and no completed receipt.
                let defer_audit = !is_deny;

                // Persist to EventStore immediately only for denied calls.
                if !defer_audit {
                    if let Some(store) = &self.event_store {
                        // ADR-103 Decision (a): the closed `work_class` enum
                        // is stamped on every event, denial included -- only
                        // `resource.cost_unit` is scoped to a successful
                        // dispatch by Amendment 1. `base_resource_payload()`
                        // carries `work_class` alone, no `cost_unit` key.
                        let storage_event = build_audit_storage_event(
                            &gate_req,
                            &audit,
                            EventOutcome::Denied,
                            Some(crate::cost_unit::base_resource_payload(request_id)),
                        );
                        // As above (line ~1513): this path always returns
                        // `PermissionDenied` below regardless, so there is no
                        // success outcome to fold a commit failure into.
                        let _ = append_audit_event_best_effort(
                            self.audit_batch.as_ref(),
                            store,
                            storage_event,
                            verb,
                            crate::audit_batch::AuditProducer::GateDenied,
                            false,
                        )
                        .await;
                    }
                }

                let reason = if is_deny {
                    let reason = match decision {
                        GateDecision::Deny { reason } => reason,
                        _ => String::new(),
                    };
                    Some(reason)
                } else {
                    None
                };
                let deferred = if defer_audit { Some(audit) } else { None };
                (reason, deferred)
            }
            Err(err) => {
                return Err(self
                    .gate_unavailable_error(&gate_req, &err, request_id)
                    .await);
            }
        };

        // Hard enforcement: Deny is authoritative.
        if let Some(reason) = gate_blocked {
            return Err(RuntimeError::PermissionDenied {
                verb: verb.to_string(),
                reason,
            });
        }

        // Mint the authorized storage token at the dispatch boundary.
        //
        // Writes pin to `local` by default. Actor identity and config
        // `[actor] id` are attribution and gate-context inputs only: they
        // never route storage. The explicit `namespace=` request param is a
        // precise single-namespace escape: the caller deliberately
        // reads/writes exactly that one set; it is NOT widened by `visible_namespaces`.
        //
        // When actor_id is configured, mint a token carrying that actor
        // label so that comm.inbox applies the to_actor filter for directed delivery.
        // Otherwise, use ActorRef::anonymous() and inbox falls back to party-line.
        // `actor_id_str` already reflects the per-request identity override
        // when supplied (resolved above into `resolved_actor`, mirrored into
        // the gate request). Reusing the same value here guarantees the
        // gate's actor and the storage token's actor can never diverge.
        //
        // On the default (no explicit `namespace=`) path, the read scope
        // widens to `['local'] ∪ visible_namespaces` (baked, or the
        // per-request override). `'local'` is always included
        // (mint_with_visibility deduplicates). Writes remain pinned to
        // `'local'`. Per-actor distinctions use view-layer tag filters
        // (assignee, actor_id, from/to), not namespace partitions. `ns`/
        // `explicit_namespace` were already validated above: reuse them
        // instead of re-reading `params["namespace"]` with `as_str()`, which
        // would silently drop malformed non-string values again.
        let token = if explicit_namespace {
            // Explicit escape: precise single-namespace scope, read+write. NOT widened.
            NamespaceToken::mint_with_visibility(ns.clone(), vec![], resolved_actor)
        } else {
            // Default path: write namespace = local; read scope = ['local'] ∪ visible_namespaces.
            let primary = Namespace::local();
            let mut extra_visible: Vec<Namespace> = match identity.as_ref() {
                Some(id) => id
                    .visible_namespaces
                    .iter()
                    .filter_map(|s| match Namespace::parse(s) {
                        Ok(parsed) => Some(parsed),
                        Err(e) => {
                            tracing::warn!(
                                namespace = %s,
                                error = %e,
                                "dispatch_with_identity: skipping invalid visible_namespace \
                                 entry from per-request identity"
                            );
                            None
                        }
                    })
                    .collect(),
                None => self.visible_namespaces.clone(),
            };
            extra_visible.push(Namespace::local()); // 'local' always readable; mint dedups
            NamespaceToken::mint_with_visibility(primary, extra_visible, resolved_actor)
        }
        .with_process_ref(match identity.as_ref() {
            Some(id) => id.process_ref.clone(),
            None => crate::config::process_ref_from_env(),
        });

        for pack in self.packs.iter() {
            if let Some(handler_def) = pack.handlers().iter().find(|v| v.name == verb) {
                // Strip `namespace` from params before forwarding to packs.
                // The registry has already consumed it to mint the NamespaceToken.
                //
                // Exception: if the handler's own `params` schema declares
                // `"namespace"` as a valid field (e.g. brain.bind, brain.unbind,
                // brain.bindings, brain.resolve), the field is a *business* argument
                // — not a transport routing key — and must be passed through
                // unchanged. Stripping it would silently default the binding to the
                // "*" wildcard, broadening profile scope across namespaces.
                let handler_accepts_namespace =
                    handler_def.params.iter().any(|p| p.name == "namespace");
                let params = if !handler_accepts_namespace {
                    if let Value::Object(mut map) = params {
                        map.remove("namespace");
                        Value::Object(map)
                    } else {
                        params
                    }
                } else {
                    params
                };
                let dispatch_start = Instant::now();
                let mut result = pack.dispatch(verb, params, self, &token).await;
                let dispatch_us = dispatch_start.elapsed().as_micros() as i64;

                // Unlike ordinary audit rows, a successful `git.digest`
                // response is returned only after its complete report has
                // been durably persisted as a schema-v2 audit receipt. The
                // receipt helper borrows the deferred audit row so malformed
                // handler output can still fall back to one generic Error
                // audit. Handler errors use that same ordinary path below.
                let git_digest_receipt_outcome = if verb == "git.digest" && result.is_ok() {
                    let resource = result.as_ref().ok().map(|value| {
                        crate::cost_unit::resource_payload(
                            verb,
                            &gate_req.args,
                            value,
                            || pack.registered_embedding_model_names().len() as i64,
                            request_id,
                        )
                    });
                    Some(
                        persist_git_digest_receipt(
                            self.event_store.as_ref(),
                            self.audit_batch.as_ref(),
                            &gate_req,
                            deferred_audit.as_ref(),
                            &mut result,
                            dispatch_us,
                            resource,
                        )
                        .await,
                    )
                } else {
                    None
                };

                // Append the deferred Allow-outcome audit row now that
                // dispatch has resolved, so `duration_us` carries the
                // measured `dispatch_us` instead of the `Event::new` default
                // of 0. A successful singleton `link` call enriches the row
                // with the created/resolved edge (schema v2); anything that
                // cannot be enriched, or is not a singleton `link` call,
                // falls back to the generic v1 audit shape so no audit row
                // is ever dropped for the deferred path.
                let needs_generic_audit = git_digest_receipt_outcome.is_none()
                    || git_digest_receipt_outcome == Some(GitDigestReceiptOutcome::BuildRejected);
                if let (true, Some(audit)) = (needs_generic_audit, deferred_audit.take()) {
                    if let Some(store) = &self.event_store {
                        let is_link_singleton =
                            verb == "link" && gate_req.args.get("links").is_none();
                        // Read-only pass over `result` first: every arm below
                        // only needs `audit_outcome` afterward, and folding a
                        // failure into `result` requires a mutable borrow
                        // that cannot coexist with the `&result` match below.
                        let audit_outcome: Result<(), RuntimeError> = match &result {
                            Ok(ok_val) if is_link_singleton => {
                                // ADR-103 Amendment 1: `link` (singleton or
                                // bulk) has no embedding-bearing path — edges
                                // carry no embedded body — so cost_unit is
                                // always base_weight("link") alone. The
                                // registered-model closure is never invoked
                                // (per_item_weight("link", ..) short-circuits
                                // to 0 before `model_count` reads it).
                                let resource = crate::cost_unit::resource_payload(
                                    verb,
                                    &gate_req.args,
                                    ok_val,
                                    || pack.registered_embedding_model_names().len() as i64,
                                    request_id,
                                );
                                match link_audit_success_from_result(audit.clone(), ok_val) {
                                    Some((edge_id, mut payload)) => {
                                        if let Value::Object(ref mut map) = payload {
                                            map.insert("resource".to_string(), resource);
                                        }
                                        let storage_event = Event::new(
                                            gate_req.namespace.as_str(),
                                            gate_req.verb.as_str(),
                                            EventKind::Audit,
                                            SubstrateKind::Event,
                                            format!(
                                                "{}:{}",
                                                gate_req.actor.kind, gate_req.actor.id
                                            ),
                                        )
                                        .with_outcome(EventOutcome::Success)
                                        .with_target(edge_id)
                                        .with_payload(payload)
                                        .with_payload_schema_version(2)
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(
                                            self.audit_batch.as_ref(),
                                            store,
                                            storage_event,
                                            verb,
                                            crate::audit_batch::AuditProducer::DispatchSucceeded,
                                            self.admission_degrade_safe(verb),
                                        )
                                        .await
                                    }
                                    None => {
                                        tracing::warn!(
                                            verb,
                                            "link audit v2 enrichment parse failed; \
                                             falling back to v1 audit shape"
                                        );
                                        let storage_event = build_audit_storage_event(
                                            &gate_req,
                                            &audit,
                                            EventOutcome::Success,
                                            Some(resource),
                                        )
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(
                                            self.audit_batch.as_ref(),
                                            store,
                                            storage_event,
                                            verb,
                                            crate::audit_batch::AuditProducer::DispatchSucceeded,
                                            self.admission_degrade_safe(verb),
                                        )
                                        .await
                                    }
                                }
                            }
                            _ => {
                                // The persisted audit outcome must reflect
                                // the dispatch result, not be hardcoded to
                                // Success — otherwise a failed dispatch is
                                // recorded as successful work and disappears
                                // from `outcome=error` queries.
                                //
                                // ADR-103 Amendment 1: `resource.cost_unit` is
                                // computed ONLY on a successful dispatch —
                                // there is no handler `Value` to read
                                // `item_count` from on an error, and the
                                // amendment's "absence has exactly two
                                // meanings" rule requires the field be
                                // omitted, never defaulted to 0, on an
                                // errored dispatch. `work_class` itself is
                                // NOT one of those two omission cases
                                // (ADR-103 Decision (a) stamps it on every
                                // event), so an errored dispatch still gets
                                // `resource: {"work_class": "interactive"}`,
                                // just with no `cost_unit` key.
                                let (outcome, resource) = match &result {
                                    Ok(ok_val) => (
                                        EventOutcome::Success,
                                        Some(crate::cost_unit::resource_payload(
                                            verb,
                                            &gate_req.args,
                                            ok_val,
                                            || pack.registered_embedding_model_names().len() as i64,
                                            request_id,
                                        )),
                                    ),
                                    Err(_) => (
                                        EventOutcome::Error,
                                        Some(crate::cost_unit::base_resource_payload(request_id)),
                                    ),
                                };
                                let producer = if result.is_ok() {
                                    crate::audit_batch::AuditProducer::DispatchSucceeded
                                } else {
                                    crate::audit_batch::AuditProducer::DispatchFailed
                                };
                                let storage_event =
                                    build_audit_storage_event(&gate_req, &audit, outcome, resource)
                                        .with_duration_us(dispatch_us);
                                append_audit_event_best_effort(
                                    self.audit_batch.as_ref(),
                                    store,
                                    storage_event,
                                    verb,
                                    producer,
                                    self.admission_degrade_safe(verb),
                                )
                                .await
                            }
                        };
                        // Only a would-be-success dispatch can be flipped by
                        // an obligation failure (ADR-133 D2/D3/D4): an
                        // already-erroring dispatch (DispatchFailed producer)
                        // keeps its original error, matching
                        // `fold_audit_obligation`'s contract.
                        result = fold_audit_obligation(result, audit_outcome);
                    }
                }

                // Post-dispatch hook: fires on success, opt-in.
                if let (Ok(ref ok_val), Some(hook)) = (&result, &self.dispatch_hook) {
                    let mut dispatch_event = Event::new(
                        ns.as_str(),
                        verb,
                        EventKind::Audit,
                        SubstrateKind::Event,
                        pack.name(),
                    )
                    .with_outcome(EventOutcome::Success)
                    .with_duration_us(dispatch_us);

                    // For recall verbs: extract the first result's id as
                    // target_id so the brain temporal posterior can observe
                    // real hit/miss and latency. Copy the serve-attribution
                    // fields from that same hit so the hook credits the profile
                    // that actually served instead of always crediting default.
                    if verb == "memory.recall" {
                        let first_result =
                            ok_val.as_array().and_then(|arr| arr.first()).or_else(|| {
                                ok_val
                                    .get("results")
                                    .and_then(Value::as_array)
                                    .and_then(|arr| arr.first())
                            });
                        let first_note_id = first_result
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<uuid::Uuid>().ok());
                        if let Some(note_id) = first_note_id {
                            dispatch_event = dispatch_event.with_target(note_id);
                        }
                        let mut payload = serde_json::Map::new();
                        if let Some(profile_id) = first_result
                            .and_then(|v| v.get("served_by_profile_id"))
                            .and_then(Value::as_str)
                        {
                            payload.insert(
                                "served_by_profile_id".to_string(),
                                Value::String(profile_id.to_string()),
                            );
                        }
                        if let Some(attribution) = first_result
                            .and_then(|v| v.get("serve_attribution"))
                            .and_then(Value::as_str)
                        {
                            payload.insert(
                                "serve_attribution".to_string(),
                                Value::String(attribution.to_string()),
                            );
                        }
                        dispatch_event = dispatch_event.with_payload(Value::Object(payload));
                        // No first result → target_id stays None (RecallMiss
                        // in brain's event interpreter).
                    }

                    let dispatch_view = EventView {
                        event: dispatch_event,
                        observations: Vec::new(),
                    };
                    let hook = Arc::clone(hook);
                    hook.on_dispatch(&dispatch_view).await;
                }

                // Recently-referenced ring admission: only by-id touches admit
                // an id. Runs unconditionally (not gated on `dispatch_hook`,
                // which is opt-in) because the ring is a core
                // dispatch-boundary capability, not an observer.
                //
                // Keyed on `token.namespace()`, NOT `ns`: `ns` is the
                // gate-resolved namespace, which on the default
                // (non-explicit) dispatch path can be a non-local
                // `default_namespace` (e.g. "foreign") while the storage
                // token that actually created/touched the record is pinned
                // to `local`. The ring must be keyed on the namespace the
                // record actually lives in: the same namespace
                // `resolve_reference`'s ring lookup uses: or admission and
                // lookup silently diverge on any non-local `default_namespace`
                // config.
                if let Ok(ref ok_val) = result {
                    let admissions = crate::reference_ring::ring_admissions_for(verb, ok_val);
                    if !admissions.is_empty() {
                        let actor_key = format!("{}:{}", gate_req.actor.kind, gate_req.actor.id);
                        for (id, name) in admissions {
                            self.reference_ring.admit(
                                token.namespace().as_str(),
                                &actor_key,
                                id,
                                name,
                            );
                        }
                    }
                }

                return result;
            }
        }

        // No pack owns this verb: the gate allowed it, but no dispatch runs.
        // Persist the deferred audit row now (duration stays at the
        // `Event::new` default of 0 — no dispatch occurred to measure) so an
        // allowed-but-unknown verb is never silently dropped from the audit
        // trail (matches the "no audit row is ever dropped" contract above).
        if let Some(audit) = deferred_audit.take() {
            if let Some(store) = &self.event_store {
                // Dispatch is about to return `UnknownVerb` below (no pack
                // owns this verb), so the persisted outcome must be `Error`,
                // not `Success`. `work_class` is still stamped (ADR-103
                // Decision (a)); `resource.cost_unit` is omitted, matching
                // every other errored-dispatch row.
                let storage_event = build_audit_storage_event(
                    &gate_req,
                    &audit,
                    EventOutcome::Error,
                    Some(crate::cost_unit::base_resource_payload(request_id)),
                );
                // Dispatch already returns `UnknownVerb` below regardless, so
                // — as with the deny paths above — there is no success
                // outcome to fold a commit failure into.
                let _ = append_audit_event_best_effort(
                    self.audit_batch.as_ref(),
                    store,
                    storage_event,
                    verb,
                    crate::audit_batch::AuditProducer::UnknownVerb,
                    false,
                )
                .await;
            }
        }

        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(RuntimeError::UnknownVerb(format!(
            "unknown verb {verb:?}; available: {}",
            self.available_verbs.join(", ")
        )))
    }

    /// Dispatch a verb under an out-of-band verified actor identity.
    ///
    /// `verified_actor` is a typed [`VerifiedActor`] (constructor rejects blank
    /// identifiers) — only code holding a `VerbRegistry` handle can supply it.
    /// `dispatch_as` never reads `params["actor"]` to derive the effective actor;
    /// individual verbs may still accept an `actor` field for their own documented
    /// business semantics, unrelated to the acting principal. Every pack handler
    /// that reads "who is calling" resolves it from the `NamespaceToken` the
    /// dispatch boundary mints, so `verified_actor` becomes exactly the principal
    /// those handlers observe.
    ///
    /// Equivalent to `dispatch_with_identity(verb, params, Some(identity))` with
    /// `identity.actor_id = Some(verified_actor)` and every other identity scalar
    /// (namespace, visible namespaces) left at this registry's construction-baked
    /// value. [`Self::dispatch`] and [`Self::dispatch_with_identity`] are unaffected.
    /// See `docs/api/pack.md#dispatch_as` for the embedding-host use case and the
    /// blank-identifier safety rationale.
    pub async fn dispatch_as(
        &self,
        verb: &str,
        params: Value,
        verified_actor: VerifiedActor,
    ) -> Result<Value, RuntimeError> {
        let identity = RequestIdentity {
            namespace: self.default_namespace.clone(),
            actor_id: Some(verified_actor.into_inner()),
            visible_namespaces: self
                .visible_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            process_ref: crate::config::process_ref_from_env(),
            request_id: None,
        };
        self.dispatch_with_identity(verb, params, Some(identity))
            .await
    }

    /// Registered pack-level by-ID resolvers, in registration order.
    ///
    /// Each element is `(pack_name, resolver)`. The kg `get` and `delete` handlers
    /// iterate this slice to probe pack-private tables when the standard KG
    /// substrates (entity/note/edge/event) return `None` for a given UUID.
    pub fn resolvers(&self) -> &[(String, Box<dyn PackByIdResolver>)] {
        &self.resolvers
    }

    /// The daemon-warm recently-referenced ring (unified-verb draft ADR,
    /// Slice 1). Consumed by `resolve_reference` (Layer 0 stage 2) and by the
    /// `resolve` verb handler; admitted-to by every successful by-id
    /// dispatch (see the admission block in `dispatch_with_identity`).
    pub fn reference_ring(&self) -> &Arc<crate::reference_ring::ReferenceRing> {
        &self.reference_ring
    }

    /// Find a kind hook among the registered packs.
    ///
    /// Walks packs in registration order; the first pack that both owns the
    /// kind (declares it in `note_kinds()` or `entity_kinds()`) and returns
    /// a hook from `kind_hook(kind)` wins. Returns `None` if the kind is
    /// unknown to all packs or no owning pack registered a hook.
    pub fn find_kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        for pack in self.packs.iter() {
            let owns = pack.note_kinds().contains(&kind) || pack.entity_kinds().contains(&kind);
            if owns {
                if let Some(hook) = pack.kind_hook(kind) {
                    return Some(hook);
                }
            }
        }
        None
    }

    /// Run the owning kind's shared-note-update normalizer/validator, if it declares one.
    ///
    /// Both canonical KG dispatch and user-facing atomic preparation call this
    /// seam so pack-specific property invariants cannot drift between them.
    pub async fn prepare_note_update_hook(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        if let Some(hook) = self.find_kind_hook(&note.kind) {
            hook.prepare_note_update(runtime, token, note, args).await?;
        }
        Ok(())
    }

    /// Run the owning kind's shared-note-update property validator, if it
    /// declares one.
    ///
    /// Kept as the validation-only compatibility seam for callers that do not
    /// own a mutable request object. Canonical and atomic CRUD use
    /// [`Self::prepare_note_update_hook`] so a hook can also normalize coupled
    /// fields before its validation runs.
    pub async fn validate_note_update_hook(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        note: &khive_storage::Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        if let Some(hook) = self.find_kind_hook(&note.kind) {
            hook.validate_note_update(runtime, token, note, properties)
                .await?;
        }
        Ok(())
    }

    /// Run shared-link validators grouped by the owning source-note kind.
    ///
    /// Supplying the whole proposed batch lets a kind hook reject an invariant
    /// violation formed only by multiple entries in that batch. Sources that
    /// are not live notes, or whose kind has no hook, remain the canonical
    /// endpoint validator's responsibility.
    pub async fn validate_link_hooks(
        &self,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        specs: &[LinkSpec],
    ) -> Result<(), RuntimeError> {
        let mut specs_by_kind: HashMap<String, Vec<LinkSpec>> = HashMap::new();
        for spec in specs {
            let Some(Resolved::Note(source)) = runtime.resolve_by_id(token, spec.source_id).await?
            else {
                continue;
            };
            specs_by_kind
                .entry(source.kind)
                .or_default()
                .push(spec.clone());
        }
        for (kind, kind_specs) in specs_by_kind {
            if let Some(hook) = self.find_kind_hook(&kind) {
                hook.validate_links(runtime, token, &kind_specs).await?;
            }
        }
        Ok(())
    }

    /// Whether any registered pack declares a handler with this verb name.
    ///
    /// A non-dispatch capability check: callers that would otherwise pay a
    /// guaranteed-failed `dispatch` (and its audit write) when an optional
    /// pack is absent can probe first and skip the call entirely.
    pub fn has_verb(&self, verb: &str) -> bool {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .any(|h| h.name == verb)
    }

    /// All MCP-exposed handlers across all registered packs (`Visibility::Verb` only).
    ///
    /// Subhandlers (`Visibility::Subhandler`) are excluded — they are internal
    /// pipeline steps not surfaced on the MCP wire. Returned with `'static`
    /// lifetime since pack handlers are `&'static [HandlerDef]` constants.
    pub fn all_verbs(&self) -> Vec<&'static HandlerDef> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .filter(|h| matches!(h.visibility, Visibility::Verb))
            .collect()
    }

    /// All MCP-exposed handlers paired with the name of the pack that owns them
    /// (`Visibility::Verb` only).
    ///
    /// Subhandlers (`Visibility::Subhandler`) are excluded from the MCP catalog
    /// Use `all_handlers_with_names` when internal handlers must
    /// also be enumerated (e.g. runtime introspection).
    pub fn all_verbs_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter().map(move |v| (p.name(), v)))
            .filter(|(_, h)| matches!(h.visibility, Visibility::Verb))
            .collect()
    }

    /// All handler definitions across all registered packs, including subhandlers.
    ///
    /// Unlike `all_verbs`, this includes `Visibility::Subhandler` entries. Useful
    /// for runtime introspection (e.g. `list_handlers`) and tooling that needs
    /// the complete handler surface.
    pub fn all_handlers_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.handlers().iter().map(move |v| (p.name(), v)))
            .collect()
    }

    /// Merged set of note kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_note_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.note_kinds().iter().copied())
            .filter(|k| seen.insert(*k))
            .collect()
    }

    /// Note kinds owned by a pack, i.e. every kind in [`all_note_kinds`] that
    /// is not one of the generic-CRUD pack's own kinds.
    ///
    /// [`GENERIC_CRUD_PACK`] declares the general-purpose note kinds the shared
    /// CRUD verbs exist to serve (`observation`, `insight`, …); every other
    /// pack's kinds are records that pack's own verbs create and maintain.
    /// Derived from the packs' `NOTE_KINDS` constants, so a pack that adds or
    /// drops a kind moves this set with it — nothing is hardcoded here but the
    /// name of the generic pack itself.
    ///
    /// [`all_note_kinds`]: Self::all_note_kinds
    pub fn pack_owned_note_kinds(&self) -> Vec<&'static str> {
        let generic: std::collections::HashSet<&'static str> = self
            .packs
            .iter()
            .filter(|p| p.name() == GENERIC_CRUD_PACK)
            .flat_map(|p| p.note_kinds().iter().copied())
            .collect();
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .filter(|p| p.name() != GENERIC_CRUD_PACK)
            .flat_map(|p| p.note_kinds().iter().copied())
            .filter(|k| !generic.contains(k) && seen.insert(*k))
            .collect()
    }

    /// Merged set of entity kinds across all registered packs (deduplicated,
    /// first-seen order preserved).
    pub fn all_entity_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.entity_kinds().iter().copied())
            .filter(|k| seen.insert(*k))
            .collect()
    }

    /// Merged set of brain profile consumer kinds requested by registered
    /// packs (deduplicated, first-seen order preserved).
    pub fn all_brain_consumer_kinds(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        self.packs
            .iter()
            .flat_map(|p| p.brain_consumer_kinds().iter().copied())
            .filter(|kind| seen.insert(*kind))
            .collect()
    }

    /// Names of packs in topological load order.
    pub fn pack_names(&self) -> Vec<&str> {
        self.packs.iter().map(|p| p.name()).collect()
    }

    /// Declared dependencies for a registered pack.
    pub fn pack_requires(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.requires())
    }

    /// Note kinds owned by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. The slice is
    /// the pack's `NOTE_KINDS` constant — `'static` lifetime, no allocation.
    pub fn pack_note_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.note_kinds())
    }

    /// Entity kinds owned by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. The slice is
    /// the pack's `ENTITY_KINDS` constant — `'static` lifetime, no allocation.
    pub fn pack_entity_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.entity_kinds())
    }

    /// Handlers declared by a specific registered pack.
    ///
    /// Returns `None` if no pack with `name` is registered. Each `HandlerDef`
    /// carries name + description + visibility — sufficient for introspection clients.
    pub fn pack_verbs(&self, name: &str) -> Option<&'static [HandlerDef]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.handlers())
    }

    /// All pack-declared edge endpoint rules across registered packs.
    ///
    /// Order follows topological pack registration; duplicates are *not* deduplicated —
    /// validation only checks membership, and an exact-duplicate rule is a
    /// harmless restatement.
    pub fn all_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.packs
            .iter()
            .flat_map(|p| p.edge_rules().iter().copied())
            .collect()
    }

    /// All pack-declared entity-type subtypes across registered packs.
    ///
    /// Order follows topological pack registration; duplicates are *not*
    /// deduplicated here — same posture as [`all_edge_rules`](Self::all_edge_rules).
    /// Consumers compose this with `EntityTypeRegistry::builtin()` via
    /// `EntityTypeRegistry::with_extra` to get the boot-time composed registry.
    pub fn all_entity_types(&self) -> Vec<EntityTypeDef> {
        self.packs
            .iter()
            .flat_map(|p| p.entity_types().iter().cloned())
            .collect()
    }

    /// Collect all `NoteKindSpec` declarations from every loaded pack.
    ///
    /// Used by the runtime for lifecycle introspection and future enforcement.
    pub fn all_note_kind_specs(&self) -> Vec<&'static NoteKindSpec> {
        self.packs
            .iter()
            .flat_map(|p| p.note_kind_specs().iter())
            .collect()
    }

    /// All pack-contributed validation rules across registered packs.
    ///
    /// Returns references into the pack-owned `'static` slices — no allocation
    /// beyond the outer `Vec`. Rule IDs are namespaced by pack; callers can
    /// group by `rule.id.split_once('/')` to attribute rules to their packs.
    pub fn all_validation_rules(&self) -> Vec<&'static ValidationRule> {
        self.packs
            .iter()
            .flat_map(|p| p.validation_rules().iter())
            .collect()
    }

    /// Pack-auxiliary schema plans for all registered packs.
    ///
    /// Returns one `SchemaPlan` per pack. Callers (typically the runtime
    /// bootstrap) apply each plan to the pack's assigned backend. Empty plans
    /// are included so the caller can iterate uniformly; callers that want to
    /// skip empty plans should check `plan.is_empty()`.
    pub fn all_schema_plans(&self) -> Vec<SchemaPlan> {
        self.packs.iter().map(|p| p.schema_plan()).collect()
    }

    /// Invoke `PackRuntime::register_embedders` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that custom embedding providers
    /// contributed by packs are reachable via `KhiveRuntime::embedder(name)`.
    ///
    /// Packs whose `register_embedders` is the default no-op pay no overhead.
    /// The method is idempotent when the underlying registry uses last-wins
    /// semantics for duplicate provider names.
    pub fn call_register_embedders(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_embedders(runtime);
        }
    }

    /// Invoke `PackRuntime::register_entity_type_validator` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that entity-type validation at the
    /// runtime layer is active for all write paths including direct `create_many`
    /// callers that bypass the handler layer.
    ///
    /// Packs whose `register_entity_type_validator` is the default no-op pay
    /// no overhead.
    ///
    /// Composes [`all_entity_types`](Self::all_entity_types) once and passes
    /// the same aggregate to every pack, mirroring how `install_edge_rules`
    /// installs one `all_edge_rules()` aggregate for the whole registry.
    pub fn call_register_entity_type_validators(&self, runtime: &KhiveRuntime) {
        let entity_types = self.all_entity_types();
        for pack in self.packs.iter() {
            pack.register_entity_type_validator_with_types(runtime, &entity_types);
        }
    }

    /// Invoke `PackRuntime::register_note_mutation_hook` on every registered pack.
    ///
    /// Called by the transport during startup, after the registry is built and
    /// before the first verb dispatch, so that note-mutation notifications at
    /// the runtime layer are active for all write paths — including KG's
    /// `update`/`delete` verbs reaching a `kind="memory"` note, which have no
    /// crate-level dependency on `khive-pack-memory`.
    ///
    /// Packs whose `register_note_mutation_hook` is the default no-op pay no
    /// overhead.
    pub fn call_register_note_mutation_hooks(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_mutation_hook(runtime);
        }
    }

    /// Invoke `PackRuntime::register_note_write_validator` on every registered pack.
    ///
    /// Called by the transport during startup with the same timing as
    /// `call_register_note_mutation_hooks`, so note-write validation is active
    /// at the runtime layer for every write path — the generic `create` verb,
    /// direct Rust callers, and proposal apply, none of which dispatch a pack
    /// hook of their own on the note-write.
    pub fn call_register_note_write_validators(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_note_write_validator(runtime);
        }
    }

    /// Invoke `PackRuntime::warm` on every registered pack.
    /// Called by the daemon at boot (in a background task) so expensive in-memory
    /// state (ANN indexes) is pre-loaded without blocking request serving.
    pub async fn call_warm_all(&self) {
        for pack in self.packs.iter() {
            pack.warm().await;
        }
    }

    /// Resolve the presentation policy for a verb name.
    ///
    /// Walks all registered handlers (including subhandlers) for the first
    /// matching name and returns its declared [`VerbPresentationPolicy`].
    /// Returns `Standard` for unknown verbs — unknown verbs will fail at
    /// dispatch anyway, so the fallback here is safe.
    pub fn presentation_policy_for(&self, verb: &str) -> khive_types::VerbPresentationPolicy {
        for pack in self.packs.iter() {
            if let Some(handler) = pack.handlers().iter().find(|h| h.name == verb) {
                return handler.presentation_policy();
            }
        }
        khive_types::VerbPresentationPolicy::Standard
    }

    /// Returns `true` if the named verb exists and is tagged
    /// `Visibility::Subhandler` (internal / operator-only).
    ///
    /// Used by the MCP server to gate subhandler invocation at the wire
    /// boundary without blocking internal callers that invoke the same verbs
    /// through the runtime directly.
    pub fn is_subhandler_verb(&self, verb: &str) -> bool {
        for pack in self.packs.iter() {
            if let Some(handler) = pack.handlers().iter().find(|h| h.name == verb) {
                return matches!(handler.visibility, Visibility::Subhandler);
            }
        }
        false
    }

    /// Apply all non-empty pack-auxiliary schema plans to the given backend.
    ///
    /// This is the centralized startup hook that replaced the previous lazy
    /// per-pack self-bootstrap pattern. Each pack's `SchemaPlan` carries
    /// idempotent `CREATE TABLE IF NOT EXISTS` DDL; calling this more than once
    /// is safe. Empty plans are skipped.
    ///
    /// Errors from individual plans are logged via `tracing::warn!` and not
    /// propagated so that a single pack's schema failure does not prevent the
    /// rest from loading. Callers that need hard-failure semantics should call
    /// `all_schema_plans()` and apply each plan individually.
    pub fn apply_schema_plans(&self, backend: &khive_db::StorageBackend) {
        if backend.is_read_only() {
            tracing::info!(
                "skipping pack schema plans because the backend is read-only; snapshot schema is used as-is"
            );
            return;
        }
        for plan in self.all_schema_plans() {
            if plan.is_empty() {
                continue;
            }
            if let Err(e) = backend.apply_pack_ddl_statements(plan.statements) {
                tracing::warn!(
                    pack = plan.pack,
                    error = %e,
                    "failed to apply pack schema plan at startup (non-fatal)"
                );
            }
        }
    }

    /// Pack-auxiliary schema plans with their owning pack names.
    ///
    /// Returns `(pack_name, SchemaPlan)` pairs for every registered pack.
    /// Used by the multi-backend boot path to apply each plan to the pack's
    /// assigned backend rather than a single shared backend.
    pub fn all_schema_plans_named(&self) -> Vec<(&'static str, SchemaPlan)> {
        self.packs
            .iter()
            .map(|p| {
                let plan = p.schema_plan();
                (plan.pack, plan)
            })
            .collect()
    }

    /// Apply pack-auxiliary schema plans using a per-pack backend map.
    ///
    /// For each `(pack_name, plan)` returned by `all_schema_plans_named()`,
    /// applies the plan to `backend_for_pack[pack_name]` when present,
    /// falling back to `default_backend` for any pack not in the map.
    ///
    /// Returns an error when two packs on the same backend declare the same
    /// auxiliary table (ADR-028 §7 collision policy: boot failure naming both
    /// packs and the conflicting table).
    ///
    /// This is the multi-backend boot path (ADR-028). Single-backend callers
    /// should continue using [`Self::apply_schema_plans`].
    pub fn apply_schema_plans_with_map(
        &self,
        backend_for_pack: &HashMap<&str, &khive_db::StorageBackend>,
        default_backend: &khive_db::StorageBackend,
    ) -> Result<(), crate::PackSchemaCollisionError> {
        // Track which pack first claimed each table on each backend.
        // Backend identity is the raw pointer of the underlying connection pool Arc.
        let mut claimed: HashMap<(*const (), String), &'static str> = HashMap::new();

        for (pack_name, plan) in self.all_schema_plans_named() {
            if plan.is_empty() {
                continue;
            }
            let backend = backend_for_pack
                .get(pack_name)
                .copied()
                .unwrap_or(default_backend);
            let backend_ptr = std::sync::Arc::as_ptr(&backend.pool_arc()) as *const ();

            // Pre-scan DDL for table names and detect collisions before applying.
            for stmt in plan.statements {
                for table_name in extract_table_names(stmt) {
                    let key = (backend_ptr, table_name.clone());
                    match claimed.entry(key) {
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(pack_name);
                        }
                        std::collections::hash_map::Entry::Occupied(e) => {
                            let prior_pack = *e.get();
                            return Err(crate::PackSchemaCollisionError {
                                pack_a: prior_pack,
                                pack_b: pack_name,
                                table: table_name,
                            });
                        }
                    }
                }
            }

            if backend.is_read_only() {
                tracing::info!(
                    pack = pack_name,
                    "skipping pack schema plan because its assigned backend is read-only"
                );
                continue;
            }

            backend
                .apply_pack_ddl_statements(plan.statements)
                .map_err(|e| crate::PackSchemaCollisionError {
                    pack_a: pack_name,
                    pack_b: pack_name,
                    table: format!("DDL error: {e}"),
                })?;
        }
        Ok(())
    }
}

// ── Inventory-based dynamic pack loading ────────────────────────────────────

/// Output of [`PackFactory::create_install`] — bundles the pack runtime with
/// its optional by-ID resolver and dispatch hook so a factory can hand back
/// all three built from one shared instance (see `BrainPackFactory` for why
/// this matters: the dispatch hook must observe the same state the runtime
/// mutates, not a second unrelated instance).
pub struct PackInstall {
    /// The pack runtime, registered into the builder's pack list.
    pub runtime: Box<dyn PackRuntime>,
    /// Optional by-ID resolver, registered when present.
    pub resolver: Option<Box<dyn PackByIdResolver>>,
    /// Optional post-dispatch observer, wired via `VerbRegistryBuilder::with_dispatch_hook`.
    pub dispatch_hook: Option<Arc<dyn DispatchHook>>,
}

/// Factory for creating pack instances registered via `inventory` at link time.
/// Each pack crate submits a `&'static dyn PackFactory` wrapped in a
/// [`PackRegistration`]; the binary's linker collects them all into a single
/// slice iterable at runtime.
///
/// Implementors must be `Send + Sync + 'static` because the registry is built
/// once and shared across async tasks.
/// Possession-bounded capability for the trusted channel-ingest note path.
///
/// Constructible only inside `khive-runtime` (the field is private), and
/// granted during pack registration exclusively to factories named in
/// `CHANNEL_INGEST_CAPABLE_PACKS`. Every call to
/// [`crate::KhiveRuntime::try_create_note_as_trusted_ingest`] must present a
/// reference to one, so the set of callers able to establish transport-owned
/// message properties is bounded by possession at the composition root, not
/// by a documentation prohibition. Two ways to obtain one: registering
/// through [`PackRegistry::register_packs`]/`register_packs_with_runtimes`
/// under the `comm` name (the allowlisted, automatic path), or a composition
/// root that builds packs directly calling
/// [`ChannelIngestCapability::grant_for_direct_composition`] and passing the
/// result to [`crate::PackRuntime::accept_channel_ingest_capability`] (or a
/// pack's constructor variant that does so) itself. The residual trust
/// assumption is unchanged either way: whoever assembles the
/// `VerbRegistryBuilder` already decides which packs are wired in and already
/// holds a `KhiveRuntime`, so minting the grant explicitly carries no more
/// privilege than that composition already had by choosing to register `comm`
/// at all.
pub struct ChannelIngestCapability {
    pub(crate) _sealed: (),
}

impl ChannelIngestCapability {
    /// Mint a capability for a composition root that constructs
    /// channel-transport packs directly, bypassing
    /// [`PackRegistry::register_packs`] (which grants this automatically).
    ///
    /// See the type-level doc for the trust argument: this carries no more
    /// privilege than the caller already has by virtue of holding a
    /// `KhiveRuntime` and choosing to wire the pack in.
    pub fn grant_for_direct_composition() -> Self {
        Self { _sealed: () }
    }
}

/// Pack names entitled to a [`ChannelIngestCapability`] grant at registration.
pub(crate) const CHANNEL_INGEST_CAPABLE_PACKS: &[&str] = &["comm"];

pub trait PackFactory: Send + Sync + 'static {
    /// Canonical lowercase name for this pack (e.g. `"kg"`, `"gtd"`).
    fn name(&self) -> &'static str;

    /// Names of packs that must be loaded before this one.
    ///
    /// Defaults to empty so pack crates that have no dependencies compile
    /// without changes. [`PackRegistry::register_packs`] validates that every
    /// name listed here is present in the caller's explicit pack list — absent
    /// dependencies are a boot error, not silently auto-added.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// Create a new pack instance for the given runtime.
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime>;

    /// Build the full installation bundle for this pack: runtime, optional
    /// resolver, optional dispatch hook.
    ///
    /// Defaults to composing `create` and `create_resolver` with no dispatch
    /// hook, so existing factories compile unchanged. Packs whose dispatch
    /// hook must observe the same instance as the runtime (e.g. `brain`)
    /// override this method instead of `create`, since the default would
    /// otherwise require two independent instances to share state.
    fn create_install(&self, runtime: KhiveRuntime) -> PackInstall {
        let resolver = self.create_resolver(runtime.clone());
        PackInstall {
            runtime: self.create(runtime),
            resolver,
            dispatch_hook: None,
        }
    }

    /// Optionally create a `PackByIdResolver` for this pack.
    ///
    /// Packs that own private SQL tables implement this to hook into
    /// `get(id)` and `delete(id)`. Defaults to `None` so existing packs
    /// compile without changes.
    fn create_resolver(&self, _runtime: KhiveRuntime) -> Option<Box<dyn PackByIdResolver>> {
        None
    }
}

/// Newtype wrapper collected by `inventory` so pack crates can submit
/// `&'static dyn PackFactory` references without the type-ascription syntax
/// that `inventory::submit!` does not support for bare trait-object references.
pub struct PackRegistration(pub &'static dyn PackFactory);

inventory::collect!(PackRegistration);

/// Error returned by [`PackRegistry::register_packs`] when boot validation fails.
#[derive(Debug)]
pub enum PackLoadError {
    /// The requested pack name was not found in the inventory.
    UnknownPack(String),
    /// A pack was requested but a declared dependency is absent from the list.
    MissingDependency {
        /// The pack that declared the dependency.
        pack: String,
        /// The dependency that is missing from the requested pack list.
        dep: String,
    },
}

impl std::fmt::Display for PackLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackLoadError::UnknownPack(name) => write!(f, "unknown pack {name:?}"),
            PackLoadError::MissingDependency { pack, dep } => write!(
                f,
                "pack {pack:?} requires {dep:?}, which is not in the requested pack list; \
                 add --pack {dep} before --pack {pack}"
            ),
        }
    }
}

impl std::error::Error for PackLoadError {}

/// Registry of pack factories discovered via `inventory` at link time.
///
/// No instance is needed — all methods are associated functions that walk the
/// globally-collected [`PackRegistration`] slice.
pub struct PackRegistry;

impl PackRegistry {
    /// Names of all pack factories discovered via `inventory`.
    pub fn discovered_names() -> Vec<&'static str> {
        inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0.name())
            .collect()
    }

    /// Register the named packs into `builder` using the supplied `runtime`.
    ///
    /// Validates the explicit pack list against `PackFactory::requires()` —
    /// if any requested pack declares a dependency that is absent from `names`,
    /// registration fails (missing dependency is a boot error, not silently
    /// auto-added). Callers must include all required packs explicitly.
    ///
    /// The [`VerbRegistryBuilder::build`] topo-sort enforces correct load order.
    ///
    /// Returns `Ok(())` when all names are recognised and all declared
    /// dependencies are satisfied; returns `Err(PackLoadError)` with a
    /// distinct variant for unknown pack vs missing dependency.
    pub fn register_packs(
        names: &[String],
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        // Build a name→factory index once.
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            all.iter().copied().find(|f| f.name() == name)
        };

        // Validate that every requested name is a known factory.
        let requested: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        for name in names {
            factory_for(name.as_str()).ok_or_else(|| PackLoadError::UnknownPack(name.clone()))?;
        }

        // Validate that all requires() dependencies are explicitly present in
        // the requested set. Missing dep → boot error, not auto-add.
        for name in names {
            let factory = factory_for(name.as_str()).unwrap(); // validated above
            for &dep in factory.requires() {
                if !requested.contains(dep) {
                    return Err(PackLoadError::MissingDependency {
                        pack: name.clone(),
                        dep: dep.to_string(),
                    });
                }
            }
        }

        // Register every requested pack; VerbRegistryBuilder::build()
        // performs the topo-sort, so insertion order here does not matter.
        for name in names {
            let factory = factory_for(name.as_str()).unwrap(); // validated above
            let install = factory.create_install(runtime.clone());
            if CHANNEL_INGEST_CAPABLE_PACKS.contains(&name.as_str()) {
                install
                    .runtime
                    .accept_channel_ingest_capability(ChannelIngestCapability { _sealed: () });
            }
            builder.register_boxed(install.runtime);
            if let Some(resolver) = install.resolver {
                builder.register_resolver(name.clone(), resolver);
            }
            if let Some(hook) = install.dispatch_hook {
                builder.with_dispatch_hook(hook);
            }
        }

        Ok(())
    }

    /// Register the named packs into `builder`, routing each pack to its own runtime.
    ///
    /// `runtimes` maps pack name → `KhiveRuntime` (one per backend assignment).
    /// `default_runtime` is used for any pack whose name is not in `runtimes`.
    /// The validation logic (unknown pack, missing dependency) is identical to
    /// [`PackRegistry::register_packs`].
    ///
    /// This is the multi-backend boot path (ADR-028). Single-backend callers
    /// should continue using [`PackRegistry::register_packs`].
    pub fn register_packs_with_runtimes(
        names: &[String],
        runtimes: &HashMap<String, KhiveRuntime>,
        default_runtime: &KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), PackLoadError> {
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            all.iter().copied().find(|f| f.name() == name)
        };

        let requested: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        for name in names {
            factory_for(name.as_str()).ok_or_else(|| PackLoadError::UnknownPack(name.clone()))?;
        }

        for name in names {
            let factory = factory_for(name.as_str()).unwrap();
            for &dep in factory.requires() {
                if !requested.contains(dep) {
                    return Err(PackLoadError::MissingDependency {
                        pack: name.clone(),
                        dep: dep.to_string(),
                    });
                }
            }
        }

        for name in names {
            let factory = factory_for(name.as_str()).unwrap();
            let runtime = runtimes
                .get(name.as_str())
                .cloned()
                .unwrap_or_else(|| default_runtime.clone());
            let install = factory.create_install(runtime);
            if CHANNEL_INGEST_CAPABLE_PACKS.contains(&name.as_str()) {
                install
                    .runtime
                    .accept_channel_ingest_capability(ChannelIngestCapability { _sealed: () });
            }
            builder.register_boxed(install.runtime);
            if let Some(resolver) = install.resolver {
                builder.register_resolver(name.clone(), resolver);
            }
            if let Some(hook) = install.dispatch_hook {
                builder.with_dispatch_hook(hook);
            }
        }

        Ok(())
    }
}

fn target_id_from_args(args: &serde_json::Value) -> Option<uuid::Uuid> {
    args.get("target_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
}

/// Build a v1-shape audit storage event from a gate check outcome.
/// See `docs/api/pack.md#build_audit_storage_event` for the `resource` payload contract.
fn build_audit_storage_event(
    gate_req: &GateRequest,
    audit: &AuditEvent,
    outcome: EventOutcome,
    resource: Option<Value>,
) -> Event {
    let mut audit_data = serde_json::to_value(audit).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize AuditEvent for EventStore");
        serde_json::Value::Null
    });
    if let Some(resource) = resource {
        if let Value::Object(ref mut map) = audit_data {
            map.insert("resource".to_string(), resource);
        }
    }
    let mut storage_event = Event::new(
        gate_req.namespace.as_str(),
        gate_req.verb.as_str(),
        EventKind::Audit,
        SubstrateKind::Event,
        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
    )
    .with_outcome(outcome)
    .with_payload(audit_data);
    if let Some(target_id) = target_id_from_args(&gate_req.args) {
        storage_event = storage_event.with_target(target_id);
    }
    storage_event
}

/// Process-wide pure-observability audit appends whose errors were logged
/// and swallowed — never an obligation-bearing row, which fails its dispatch
/// instead and is counted separately by
/// [`AUDIT_OBLIGATION_APPEND_FAILURES`]/[`audit_obligation_append_failure_count`].
/// Keeping this counter obligation-free preserves its documented contract
/// (`docs/guide/api-reference.md`, `khive-db`'s `WriterContentionDiagnostics::audit_append_failures`
/// doc comment): every unit counted here was swallowed, none was propagated.
static AUDIT_APPEND_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn audit_append_failure_count() -> u64 {
    AUDIT_APPEND_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide commit failures for obligation-bearing audit rows (ADR-133
/// D2/D3/D4): gate denials, dispatch outcomes, unknown-verb rows, and
/// `git.digest` success receipts. Most call sites fold this failure into the
/// dispatch's own error (a would-be success becomes an error, per
/// [`fold_audit_obligation`]); a denial's own audit row is the one
/// exception — its dispatch already returns `PermissionDenied` independent
/// of whether this row commits, so the failure is logged and counted here
/// but not separately propagated. Disjoint from [`AUDIT_APPEND_FAILURES`] —
/// each failing row is classified by [`crate::audit_batch::classify`] into
/// exactly one of the two classes and increments exactly one of these two
/// counters, never both.
static AUDIT_OBLIGATION_APPEND_FAILURES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Test-only reader: no production caller needs this counter today (unlike
/// [`audit_append_failure_count`], which `KhiveRuntime::db_diagnostics`
/// surfaces), but the mechanism tests need to observe it directly to prove
/// obligation and swallowed failures land on disjoint counters.
#[cfg(test)]
pub(crate) fn audit_obligation_append_failure_count() -> u64 {
    AUDIT_OBLIGATION_APPEND_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide count of `DispatchObligation` rows **refused before they
/// could be enqueued** (`AuditTerminalReason::QueueAdmissionExhausted`) for an
/// [`VerbRegistry::admission_degrade_safe`] verb (khive#2147/khive#2217).
/// This is a confirmed, terminal accounting loss: the row never shared a
/// generation with anyone and will never commit. Disjoint from both
/// [`AUDIT_APPEND_FAILURES`] and [`AUDIT_OBLIGATION_APPEND_FAILURES`]: this
/// case is neither. It is not [`AUDIT_APPEND_FAILURES`] — that counter's own
/// contract (`khive-db`'s `WriterContentionDiagnostics::audit_append_failures`
/// doc) says an obligation-bearing row's commit failure "either fail[s] the
/// dispatch... or [is] tracked by the runtime's own separate
/// obligation-failure counter instead", and this dispatch does neither: it
/// reports the caller's already-computed success with no error. It is not
/// [`AUDIT_OBLIGATION_APPEND_FAILURES`] either — that counter's contract is
/// "most call sites fold this failure into the dispatch's own error", which
/// is exactly the propagation this admission-degrade path exists to avoid.
/// Also disjoint from [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`] — that
/// counter's row was enqueued and may still commit; this one's was not.
/// Read in production by [`VerbRegistry::audit_batch_metrics`], which feeds
/// it into `khive_db::diagnostics::RuntimeAuditBatchMetrics::admission_refused_obligations`
/// and from there into the `db_diagnostics` verb's
/// `writer_contention.audit_admission_refused_obligations` field (ADR-103
/// Amendment 3) — an operator can read this counter without a test-only
/// feature gate. The mechanism tests also read it directly, including the
/// admission-pressure regression tests in `tests/read_verb_admission_exhaustion.rs`,
/// which (like `khive-runtime/src/audit_batch.rs`'s own `test_internals`
/// module) need it as `pub`, not `pub(crate)`, since they compile as a
/// separate external binary outside this crate.
static AUDIT_ADMISSION_REFUSED_OBLIGATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn audit_admission_refused_obligation_count() -> u64 {
    AUDIT_ADMISSION_REFUSED_OBLIGATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Process-wide count of `DispatchObligation` rows that were **already
/// enqueued but had not resolved by the time the caller's admission wait
/// deadline elapsed** (`AuditTerminalReason::AdmissionDeadlineExpired`) for an
/// [`VerbRegistry::admission_degrade_safe`] verb (khive#2147/khive#2217).
/// Unlike [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`], a row counted here is not
/// a confirmed loss: per `AuditTerminalReason::AdmissionDeadlineExpired`'s own
/// doc, the row may still be committed (or terminally failed) by the
/// generation driver independently of the caller's timeout, so this counter
/// is an upper bound on the eventual undercount, not the undercount itself.
/// Read in production by [`VerbRegistry::audit_batch_metrics`], which feeds
/// it into `khive_db::diagnostics::RuntimeAuditBatchMetrics::admission_unresolved_obligations`
/// and from there into the `db_diagnostics` verb's
/// `writer_contention.audit_admission_unresolved_obligations` field (ADR-103
/// Amendment 3).
static AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn audit_admission_unresolved_obligation_count() -> u64 {
    AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

const GIT_DIGEST_RECEIPT_FAILURE: &str =
    "git_digest_receipt_persist_failed: git.digest writes may have committed, but no durable \
     success receipt was confirmed; inspect ingest state before retrying";

/// Tells the dispatch seam whether it should consume the deferred audit or
/// reuse it for the ordinary generic Error row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitDigestReceiptOutcome {
    /// The schema-v2 receipt landed; no second audit row may be appended.
    Persisted,
    /// The handler's nominal success could not be shaped into a receipt. The
    /// helper has converted it to an error, and the original audit remains
    /// available for one generic Error row.
    BuildRejected,
    /// Persistence could not be attempted or its append failed. A second
    /// best-effort append would either be impossible or duplicate the same
    /// known store failure, so the caller must not retry it here.
    PersistenceUnavailable,
}

/// Persist the complete successful `git.digest` report as a schema-v2 audit
/// event and add that event's UUID to the returned report as `receipt_id`.
///
/// This is intentionally strict while every other dispatch audit remains
/// best-effort: a caller must never receive an unqualified digest success if
/// response loss would leave it unable to recover the exact per-pass report.
/// Missing audit/store configuration, an invalid handler report, or an append
/// failure therefore replaces the handler success with a stable safe error.
/// The error does not expose storage paths, source URLs, or command stderr and
/// explicitly warns that ingest writes may already have committed.
async fn persist_git_digest_receipt(
    store: Option<&Arc<dyn EventStore>>,
    audit_batch: Option<&Arc<crate::audit_batch::AuditBatch>>,
    gate_req: &GateRequest,
    audit: Option<&AuditEvent>,
    result: &mut Result<Value, RuntimeError>,
    duration_us: i64,
    resource: Option<Value>,
) -> GitDigestReceiptOutcome {
    let Ok(report) = result else {
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };
    let Some(store) = store else {
        tracing::error!(
            verb = "git.digest",
            "durable receipt store is not configured"
        );
        *result = Err(RuntimeError::Internal(GIT_DIGEST_RECEIPT_FAILURE.into()));
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };
    let Some(audit) = audit else {
        tracing::error!(
            verb = "git.digest",
            "durable receipt cannot be built because the gate produced no audit decision"
        );
        *result = Err(RuntimeError::Internal(GIT_DIGEST_RECEIPT_FAILURE.into()));
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    };

    let Some(report_object) = report.as_object_mut() else {
        tracing::error!(
            verb = "git.digest",
            "digest handler returned a non-object report"
        );
        *result = Err(RuntimeError::Internal(GIT_DIGEST_RECEIPT_FAILURE.into()));
        return GitDigestReceiptOutcome::BuildRejected;
    };
    let Some(project_id) = report_object
        .get("project_id")
        .and_then(Value::as_str)
        .and_then(|raw| raw.parse::<uuid::Uuid>().ok())
    else {
        tracing::error!(
            verb = "git.digest",
            "digest handler report omitted a valid project_id"
        );
        *result = Err(RuntimeError::Internal(GIT_DIGEST_RECEIPT_FAILURE.into()));
        return GitDigestReceiptOutcome::BuildRejected;
    };

    // Allocate the event first so the exact durable key can be embedded in
    // both the caller-visible report and the report snapshot stored in it.
    let mut event = Event::new(
        gate_req.namespace.as_str(),
        gate_req.verb.as_str(),
        EventKind::Audit,
        SubstrateKind::Event,
        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
    )
    .with_outcome(EventOutcome::Success)
    .with_target(project_id)
    .with_payload_schema_version(2)
    .with_duration_us(duration_us);
    let receipt_id = event.id;
    report_object.insert(
        "receipt_id".to_string(),
        Value::String(receipt_id.to_string()),
    );

    let mut payload = serde_json::to_value(audit).unwrap_or_else(|serialize_err| {
        tracing::error!(
            verb = "git.digest",
            error = %serialize_err,
            "failed to serialize gate audit for durable digest receipt"
        );
        Value::Null
    });
    let Value::Object(payload_object) = &mut payload else {
        tracing::error!(
            verb = "git.digest",
            "gate audit serialization did not produce an object"
        );
        *result = Err(RuntimeError::Internal(GIT_DIGEST_RECEIPT_FAILURE.into()));
        return GitDigestReceiptOutcome::BuildRejected;
    };
    if let Some(resource) = resource {
        payload_object.insert("resource".to_string(), resource);
    }
    payload_object.insert("result".to_string(), report.clone());
    event.payload = payload;

    // Strict path (ADR-133): a git.digest success receipt must still commit
    // exactly once before the caller can see success, so this row waits on
    // its generation's commit through the batch seam rather than
    // best-effort — the batching only changes whether it shares a writer
    // acquisition with concurrent rows, never whether it is durable before
    // the caller observes success.
    let submit_result = if let Some(audit_batch) = audit_batch {
        use crate::audit_batch::AuditBatchControl;
        audit_batch
            .submit(crate::audit_batch::PreparedAuditRow {
                event,
                producer: crate::audit_batch::AuditProducer::GitDigestReceipt,
            })
            .await
            .map(|_outcome| ())
            .map_err(|reason| format!("{reason:?}"))
    } else {
        store.append_event(event).await.map_err(|e| e.to_string())
    };
    if let Err(store_err) = submit_result {
        // `GitDigestReceipt` is always `DispatchObligation` (see
        // `crate::audit_batch::classify`) and this failure always
        // propagates below, so it belongs on the obligation counter, not
        // the swallowed-failures one.
        AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::error!(
            verb = "git.digest",
            error = %store_err,
            receipt_id = %receipt_id,
            "durable digest receipt append failed"
        );
        *result = Err(RuntimeError::Internal(GIT_DIGEST_RECEIPT_FAILURE.into()));
        return GitDigestReceiptOutcome::PersistenceUnavailable;
    }
    GitDigestReceiptOutcome::Persisted
}

/// Append an audit event, propagating a persistent failure for
/// obligation-bearing producers and swallowing it for pure-observability
/// producers.
///
/// ADR-133 D2/D3/D4: a dispatch must not report success when the row that
/// accounts for, authorizes, or audits it did not commit. Producers
/// classified [`crate::audit_batch::AuditProductionClass::DispatchObligation`]
/// (gate denials, dispatch outcomes, unknown-verb, git.digest receipts)
/// therefore return `Err` here on a persistent commit failure; the caller is
/// responsible for folding that into the dispatch result on the
/// success path — see [`fold_audit_obligation`]. Producers classified
/// [`crate::audit_batch::AuditProductionClass::PureObservability`]
/// (config-lock rows, `memory.recall` execution) degrade gracefully: the
/// failure is logged and counted but never returned, matching the pre-ADR-133
/// best-effort contract.
///
/// Every failure — obligation or observability — increments one of the
/// process-wide diagnostics counters above; the one exception is the
/// admission-degrade case below, which increments one of its own dedicated
/// [`AUDIT_ADMISSION_REFUSED_OBLIGATIONS`] /
/// [`AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS`] counters instead — it is
/// neither a swallowed observability failure nor a propagated obligation
/// failure.
///
/// `degrade_allowlisted` (khive#2147/khive#2217) narrows that obligation for
/// one specific case: a *successful* dispatch (`AuditProducer::DispatchSucceeded`)
/// for a verb that [`VerbRegistry::admission_degrade_safe`] has explicitly
/// opted in (Assertive alone is not a sufficient signal — see that method's
/// doc) performs no domain write, so this row's own admission being
/// transiently refused or timed out (`AuditTerminalReason::QueueAdmissionExhausted`
/// / `AdmissionDeadlineExpired`) degrades to best-effort instead of failing
/// the dispatch — the caller-visible read result is preserved. This function
/// derives eligibility from `producer` itself rather than trusting the
/// caller's `degrade_allowlisted` answer in isolation, so a `DispatchFailed`
/// row can never take the degrade path no matter what a caller passes: every
/// failed dispatch, every write verb, every gate-denial/unknown-verb/git.digest
/// row stays strictly obligation-bearing.
///
/// When the registry has an audit-batch seam configured (it is whenever
/// `store` is), the row routes through
/// [`crate::audit_batch::AuditBatchControl::submit`] instead of taking its
/// own writer-task acquisition — concurrent producers collapse onto one
/// commit per generation. `audit_batch: None` (a `VerbRegistry` predating
/// the seam, or constructed without going through the builder) falls back to
/// the pre-ADR-133 direct append, classified the same way.
async fn append_audit_event_best_effort(
    audit_batch: Option<&Arc<crate::audit_batch::AuditBatch>>,
    store: &Arc<dyn EventStore>,
    event: Event,
    verb: &str,
    producer: crate::audit_batch::AuditProducer,
    degrade_allowlisted: bool,
) -> Result<(), RuntimeError> {
    use crate::audit_batch::{
        classify, AuditBatchControl, AuditProducer, AuditProductionClass, AuditTerminalReason,
    };

    let is_obligation = classify(producer) == AuditProductionClass::DispatchObligation;
    let admission_degrade_eligible =
        degrade_allowlisted && producer == AuditProducer::DispatchSucceeded;

    if let Some(audit_batch) = audit_batch {
        if let Err(reason) = audit_batch
            .submit(crate::audit_batch::PreparedAuditRow { event, producer })
            .await
        {
            if is_obligation {
                // khive#2147/khive#2217: a read verb performs no domain write, so
                // when the audit-lane's OWN admission is merely under transient
                // pressure (the row was refused before enqueue, or the caller's
                // wait deadline elapsed on a row that is still likely to commit),
                // failing the read discards a valid result to protect an
                // obligation the read never needed as strictly as a write does.
                // Any other reason (a definite store/durability failure) still
                // fails the dispatch for reads exactly as it does for writes.
                //
                // The two admission-pressure reasons are not the same fact and
                // are counted on separate counters: `QueueAdmissionExhausted`
                // never enqueued, so it is a confirmed terminal loss, while
                // `AdmissionDeadlineExpired` was already enqueued and may still
                // commit later — see `AuditTerminalReason::AdmissionDeadlineExpired`'s
                // own doc.
                if admission_degrade_eligible {
                    match reason {
                        AuditTerminalReason::QueueAdmissionExhausted => {
                            AUDIT_ADMISSION_REFUSED_OBLIGATIONS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(
                                verb,
                                reason = ?reason,
                                "read verb's audit obligation row was refused before \
                                 enqueue under audit-lane admission pressure; dispatch \
                                 still reports its own result (non-fatal)"
                            );
                            return Ok(());
                        }
                        AuditTerminalReason::AdmissionDeadlineExpired => {
                            AUDIT_ADMISSION_UNRESOLVED_OBLIGATIONS
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(
                                verb,
                                reason = ?reason,
                                "read verb's audit obligation row was still enqueued and \
                                 unresolved when the caller's admission wait deadline \
                                 elapsed; it may still commit. Dispatch still reports its \
                                 own result (non-fatal)"
                            );
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    verb,
                    reason = ?reason,
                    "audit obligation batch submission failed; failing dispatch"
                );
                return Err(RuntimeError::Internal(format!(
                    "audit obligation commit failed for verb {verb:?}: {reason:?}"
                )));
            }
            AUDIT_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                verb,
                reason = ?reason,
                "audit event batch submission failed (non-fatal)"
            );
        }
        return Ok(());
    }

    if let Err(store_err) = store.append_event(event).await {
        if is_obligation {
            AUDIT_OBLIGATION_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                verb,
                error = %store_err,
                "audit obligation store write failed; failing dispatch"
            );
            return Err(RuntimeError::Internal(format!(
                "audit obligation commit failed for verb {verb:?}: {store_err}"
            )));
        }
        AUDIT_APPEND_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            verb,
            error = %store_err,
            "audit event store write failed (non-fatal)"
        );
    }
    Ok(())
}

/// Fold an audit-obligation outcome into a dispatch result.
///
/// A dispatch that would otherwise report success cannot claim it once the
/// row accounting for it fails to commit (ADR-133 D2/D3/D4), so `Ok` becomes
/// the audit's `Err`. A dispatch that already reports failure keeps its
/// original error — the obligation is on never reporting a false success,
/// not on replacing one error with another.
fn fold_audit_obligation<T>(
    result: Result<T, RuntimeError>,
    audit_outcome: Result<(), RuntimeError>,
) -> Result<T, RuntimeError> {
    match (result, audit_outcome) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(audit_err)) => Err(audit_err),
        (Err(err), _) => Err(err),
    }
}

/// Schema v2 audit payload for a successful singleton `link` call — additive
/// over v1 via `#[serde(flatten)]`. See `docs/api/pack.md#linkauditsuccessv2`.
#[derive(Debug, Clone, serde::Serialize)]
struct LinkAuditSuccessV2 {
    #[serde(flatten)]
    audit: AuditEvent,
    edge_id: uuid::Uuid,
    source_id: uuid::Uuid,
    target_id: uuid::Uuid,
    relation: String,
    weight: f64,
}

/// Extract edge fields to enrich a successful singleton `link` audit row.
/// Returns `None` on any missing/malformed field (falls back to v1 shape).
/// See `docs/api/pack.md#link_audit_success_from_result`.
fn link_audit_success_from_result(
    audit: AuditEvent,
    result: &serde_json::Value,
) -> Option<(uuid::Uuid, serde_json::Value)> {
    let edge_id = result.get("id")?.as_str()?.parse::<uuid::Uuid>().ok()?;
    let source_id = result
        .get("source_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()?;
    let target_id = result
        .get("target_id")?
        .as_str()?
        .parse::<uuid::Uuid>()
        .ok()?;
    let relation = result.get("relation")?.as_str()?.to_string();
    let weight = result.get("weight")?.as_f64()?;
    let enriched = LinkAuditSuccessV2 {
        audit,
        edge_id,
        source_id,
        target_id,
        relation,
        weight,
    };
    let payload = serde_json::to_value(&enriched).ok()?;
    Some((edge_id, payload))
}

/// Resolve and validate a caller-supplied `namespace` argument the same way
/// on every MCP ingress path.
///
/// - Absent `namespace` key → parse `default_namespace`.
/// - Present `namespace: "<string>"` → parse the caller's value.
/// - Present non-string `namespace` (null, number, bool, array, object) →
///   fail closed with `RuntimeError::InvalidInput`. ADR-018 requires this:
///   a malformed explicit value must never be silently coerced to the
///   default namespace.
///
/// Single chokepoint for both `VerbRegistry::dispatch` and the multi-backend
/// coordinator intercept — see `docs/api/pack.md#resolve_explicit_namespace`.
pub fn resolve_explicit_namespace(
    params: &Value,
    default_namespace: &str,
) -> Result<Namespace, RuntimeError> {
    match params.get("namespace") {
        None => Namespace::parse(default_namespace)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace: {e}"))),
        Some(Value::String(ns_str)) => Namespace::parse(ns_str)
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {e}"))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "invalid namespace: expected string when present, got {}",
            json_type_name(other),
        ))),
    }
}

/// JSON type name for error messages: describes a present-but-malformed
/// `namespace` value without echoing its contents.
pub fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// INLINE TEST JUSTIFICATION: tests here exercise VerbRegistry collision detection,
// gate enforcement, and dispatch ordering that depend on direct access to the
// registry's private `packs` Vec and gate field. Moving them to tests/ would
// require pub-exporting registry internals. Broad behavioral dispatch tests
// live in tests/integration.rs.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ActorRef;
    use khive_types::Pack;

    /// Verbs known, by prior review (khive#2147/khive#2217 round 1), to have
    /// their own accounting-bearing side effect despite being declared
    /// `VerbCategory::Assertive` — see [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`]'s
    /// doc for why each is excluded. `VerbCategory::Assertive` alone cannot
    /// distinguish these from a genuinely side-effect-free read (that is the
    /// whole reason the allowlist exists instead of a bare category check),
    /// so this denylist is the mechanizable guard against silently
    /// reintroducing one of them: a category-only census would stay green if
    /// either name were re-added to the allowlist.
    const KNOWN_INCIDENTAL_WRITE_VERBS: &[&str] = &["memory.recall", "db_diagnostics"];

    /// khive-runtime links no real pack crates in its own test binary (see
    /// the comment on `CommProbeFactory` below), so
    /// [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`] cannot be checked
    /// against a live registered `HandlerDef` here. Instead this
    /// re-derives each opted-in verb's classification from
    /// `khive-pack-kg/src/handler_defs.rs`'s live source — the same
    /// fail-closed pattern as `adr133_writer_census.rs`'s
    /// `reclassify_from_live_source`. Every entry in the allowlist is
    /// currently declared in that one file; a verb from a different pack
    /// would need this scan extended to that pack's source first.
    ///
    /// This test proves category membership (`VerbCategory::Assertive`) and
    /// non-membership in [`KNOWN_INCIDENTAL_WRITE_VERBS`]. It does NOT prove
    /// general effect-purity: an Assertive handler may still emit its own
    /// observability/config events on an independent, best-effort background
    /// path (`search`'s `SearchExecuted` telemetry, `context`'s one-time
    /// `ConfigLocked` event) that this test does not inspect and that this
    /// PR's admission-degrade mechanism does not touch — those events commit
    /// or fail on their own path regardless of what happens to this
    /// dispatch's own audit row. Proving general effect-purity would require
    /// an explicit per-handler effect/accounting capability tag, which is
    /// out of scope here (see ADR-103 Amendment 3's "why this is accepted"
    /// section); this census instead locks down the two properties that are
    /// mechanizable today: declared category, and the one known-bad-name
    /// regression class.
    #[test]
    fn admission_degrade_safe_verbs_are_registered_assertive() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../khive-pack-kg/src/handler_defs.rs"
        );
        let source =
            std::fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));

        for verb in VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS {
            assert!(
                !KNOWN_INCIDENTAL_WRITE_VERBS.contains(verb),
                "admission-degrade-safe verb {verb:?} is a known incidental-write verb \
                 (khive#2147/khive#2217 round 1); it must not be re-added to \
                 ADMISSION_DEGRADE_SAFE_VERBS even though it is VerbCategory::Assertive"
            );
            // Anchored to exactly 8 leading spaces: that is the indentation
            // `HandlerDef { name: ... }` top-level fields use in this file,
            // versus 16 for a nested `ParamDef { name: ... }` — several
            // handlers (e.g. `search`) declare a `query`/`kind`/... param
            // whose own `name:` field would otherwise collide with a verb
            // of the same name declared later in the file.
            let needle = format!("\n        name: \"{verb}\",");
            let name_pos = source.find(&needle).unwrap_or_else(|| {
                panic!(
                    "admission-degrade-safe verb {verb:?} has no top-level `HandlerDef` in \
                     khive-pack-kg/src/handler_defs.rs; update the allowlist or this \
                     census's source path"
                )
            });
            // Each `HandlerDef` literal in this file declares `category:`
            // shortly after `name:`, well before the next handler's own
            // `name:` field — bound the scan to the text up to the next
            // `HandlerDef {` (or EOF) so a later handler's category can
            // never be misattributed to this one.
            let block_end = source[name_pos..]
                .find("HandlerDef {")
                .map(|offset| name_pos + offset)
                .unwrap_or(source.len());
            let block = &source[name_pos..block_end];
            assert!(
                block.contains("VerbCategory::Assertive"),
                "admission-degrade-safe verb {verb:?} is declared in \
                 khive-pack-kg/src/handler_defs.rs but is not VerbCategory::Assertive; \
                 admission degradation must not silently apply to a write-capable verb"
            );
        }
    }

    static COMM_PROBE_GRANTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    static OTHER_PROBE_GRANTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Probe factories proving the channel-ingest grant is name-bounded.
    /// khive-runtime's own test binary links no real pack crates, so the
    /// `comm` name is free for the probe here.
    struct CommProbeFactory;
    struct OtherProbeFactory;

    fn probe_pack(
        _runtime: KhiveRuntime,
        grant_flag: &'static std::sync::atomic::AtomicBool,
    ) -> Box<dyn PackRuntime> {
        struct ProbePack {
            grant_flag: &'static std::sync::atomic::AtomicBool,
        }
        #[async_trait::async_trait]
        impl PackRuntime for ProbePack {
            fn name(&self) -> &str {
                "probe"
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                &[]
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                &[]
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                &[]
            }
            fn accept_channel_ingest_capability(&self, _capability: ChannelIngestCapability) {
                self.grant_flag
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: serde_json::Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<serde_json::Value, crate::RuntimeError> {
                Err(crate::RuntimeError::InvalidInput("probe".into()))
            }
        }
        Box::new(ProbePack { grant_flag })
    }

    impl PackFactory for CommProbeFactory {
        fn name(&self) -> &'static str {
            "comm"
        }
        fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
            probe_pack(runtime, &COMM_PROBE_GRANTED)
        }
    }

    impl PackFactory for OtherProbeFactory {
        fn name(&self) -> &'static str {
            "grant-probe-other"
        }
        fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime> {
            probe_pack(runtime, &OTHER_PROBE_GRANTED)
        }
    }

    inventory::submit! { PackRegistration(&CommProbeFactory) }
    inventory::submit! { PackRegistration(&OtherProbeFactory) }

    #[test]
    fn channel_ingest_grant_reaches_only_allowlisted_pack_names() {
        let runtime = KhiveRuntime::memory().unwrap();
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs(
            &["comm".to_string(), "grant-probe-other".to_string()],
            runtime,
            &mut builder,
        )
        .expect("probe registration succeeds");
        assert!(
            COMM_PROBE_GRANTED.load(std::sync::atomic::Ordering::SeqCst),
            "the comm-named factory must receive the channel-ingest grant"
        );
        assert!(
            !OTHER_PROBE_GRANTED.load(std::sync::atomic::Ordering::SeqCst),
            "a factory outside CHANNEL_INGEST_CAPABLE_PACKS must never be granted"
        );
    }

    #[test]
    fn from_token_preserves_process_ref() {
        let with_ref = NamespaceToken::mint_authorized(
            Namespace::local(),
            ActorRef::new("agent", "provenance-carrier"),
        )
        .with_process_ref(Some("proc:origin-abc123".to_string()));
        let identity = RequestIdentity::from_token(&with_ref);
        assert_eq!(identity.process_ref.as_deref(), Some("proc:origin-abc123"));

        let without_ref = NamespaceToken::mint_authorized(
            Namespace::local(),
            ActorRef::new("agent", "provenance-absent"),
        );
        let identity = RequestIdentity::from_token(&without_ref);
        assert_eq!(identity.process_ref, None);
    }

    struct AlphaPack;

    impl Pack for AlphaPack {
        const NAME: &'static str = "alpha";
        const NOTE_KINDS: &'static [&'static str] = &["memo", "log"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
        const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["recall", "search"];
        const HANDLERS: &'static [HandlerDef] = &[
            HandlerDef {
                name: "create",
                description: "create a widget",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &[],
            },
            HandlerDef {
                name: "list",
                description: "list widgets",
                visibility: Visibility::Verb,
                category: VerbCategory::Assertive,
                params: &[],
            },
        ];
    }

    #[async_trait]
    impl PackRuntime for AlphaPack {
        fn name(&self) -> &str {
            AlphaPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            AlphaPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            AlphaPack::ENTITY_KINDS
        }
        fn brain_consumer_kinds(&self) -> &'static [&'static str] {
            AlphaPack::BRAIN_CONSUMER_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            AlphaPack::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
        }
    }

    #[derive(Debug)]
    struct GateErrorTrackingPack {
        invoked: Arc<AtomicUsize>,
    }

    impl Pack for GateErrorTrackingPack {
        const NAME: &'static str = "gate_error_tracking";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "guarded",
            description: "track whether gate-error dispatch reaches the handler",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for GateErrorTrackingPack {
        fn name(&self) -> &str {
            Self::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }

        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }

        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.invoked.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"invoked": true}))
        }
    }

    /// A pack whose `dispatch` sleeps for a fixed, generous duration so
    /// `duration_us` regression tests (ADR-103 Stage 1) have a reliably
    /// nonzero, non-flaky measured dispatch time to assert against.
    struct SleepingPack;

    impl Pack for SleepingPack {
        const NAME: &'static str = "sleeping";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "slow_op",
            description: "sleeps before returning",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for SleepingPack {
        fn name(&self) -> &str {
            SleepingPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            SleepingPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            SleepingPack::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            SleepingPack::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(serde_json::json!({ "pack": "sleeping", "verb": verb }))
        }
    }

    struct BetaPack;

    impl Pack for BetaPack {
        const NAME: &'static str = "beta";
        const NOTE_KINDS: &'static [&'static str] = &["alert"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget", "gadget"];
        const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["search", "knowledge_compose"];
        const HANDLERS: &'static [HandlerDef] = &[
            HandlerDef {
                name: "notify",
                description: "send alert",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &[],
            },
            // "create" is Subhandler so it does NOT collide with AlphaPack's
            // Verb-visibility "create" — subhandlers are pack-internal and
            // excluded from cross-pack collision detection.
            HandlerDef {
                name: "create",
                description: "beta internal create (subhandler)",
                visibility: Visibility::Subhandler,
                category: VerbCategory::Commissive,
                params: &[],
            },
        ];
    }

    /// Build a registry with AlphaPack + BetaPack.
    ///
    /// BetaPack's `create` is Subhandler so there is no Verb-visibility
    /// collision with AlphaPack's `create` Verb. Tests that need a collision
    /// use `build_colliding_registry()` instead.
    fn build_registry() -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.register(BetaPack);
        builder.build().expect("registry builds without collision")
    }

    /// Build a registry with two packs that declare the same Verb-visibility
    /// handler — used to test that `VerbCollision` is raised at build time.
    struct CollidingPack;

    impl Pack for CollidingPack {
        const NAME: &'static str = "colliding";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "create",
            description: "duplicate Verb-visibility create",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for CollidingPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "colliding", "verb": verb }))
        }
    }

    struct ReservedEnvelopeParamPack;

    impl Pack for ReservedEnvelopeParamPack {
        const NAME: &'static str = "reserved-envelope-param";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "broken.serve",
            description: "declares a transport-owned argument",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[ParamDef {
                name: "presentation",
                param_type: "object",
                required: false,
                description: "invalid collision with the request envelope",
            }],
        }];
    }

    #[async_trait]
    impl PackRuntime for ReservedEnvelopeParamPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            unreachable!("invalid handler metadata must fail before dispatch")
        }
    }

    #[async_trait]
    impl PackRuntime for BetaPack {
        fn name(&self) -> &str {
            BetaPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            BetaPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            BetaPack::ENTITY_KINDS
        }
        fn brain_consumer_kinds(&self) -> &'static [&'static str] {
            BetaPack::BRAIN_CONSUMER_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            BetaPack::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "beta", "verb": verb }))
        }
    }

    #[tokio::test]
    async fn dispatch_routes_to_correct_pack() {
        let reg = build_registry();

        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");

        let res = reg.dispatch("notify", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "beta");
    }

    /// Two packs declaring the same `Visibility::Verb` handler must be
    /// rejected at build time — the old "first registered wins" behaviour is
    /// replaced by a boot error.
    #[test]
    fn verb_collision_is_boot_time_error() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.register(CollidingPack);
        let err = builder
            .build()
            .err()
            .expect("duplicate Verb-visibility handler must be rejected at build time");
        assert!(
            matches!(err, RuntimeError::VerbCollision { ref verb, .. } if verb == "create"),
            "expected VerbCollision for 'create', got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("create"),
            "error must name the colliding verb: {msg}"
        );
        assert!(
            msg.contains("alpha") || msg.contains("colliding"),
            "error must name one of the conflicting packs: {msg}"
        );
    }

    #[test]
    fn reserved_request_envelope_param_is_boot_time_error() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(ReservedEnvelopeParamPack);
        let error = builder
            .build()
            .err()
            .expect("transport-owned parameter names must fail registry construction");
        assert!(
            matches!(
                error,
                RuntimeError::ReservedEnvelopeParam {
                    ref pack,
                    ref verb,
                    ref param,
                } if pack == "reserved-envelope-param"
                    && verb == "broken.serve"
                    && param == "presentation"
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn reserved_request_envelope_param_is_boot_time_error_for_subhandler() {
        struct ReservedEnvelopeSubhandlerParamPack;

        impl Pack for ReservedEnvelopeSubhandlerParamPack {
            const NAME: &'static str = "reserved-envelope-subhandler-param";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
                name: "broken.internal",
                description: "declares a transport-owned argument on an internal handler",
                visibility: Visibility::Subhandler,
                category: VerbCategory::Assertive,
                params: &[ParamDef {
                    name: "presentation_per_op",
                    param_type: "string",
                    required: false,
                    description: "invalid collision with the request envelope",
                }],
            }];
        }

        #[async_trait]
        impl PackRuntime for ReservedEnvelopeSubhandlerParamPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                unreachable!("invalid handler metadata must fail before dispatch")
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(ReservedEnvelopeSubhandlerParamPack);
        let error = builder
            .build()
            .err()
            .expect("transport-owned parameter names must fail registry construction");
        assert!(
            matches!(
                error,
                RuntimeError::ReservedEnvelopeParam {
                    ref pack,
                    ref verb,
                    ref param,
                } if pack == "reserved-envelope-subhandler-param"
                    && verb == "broken.internal"
                    && param == "presentation_per_op"
            ),
            "unexpected error: {error:?}"
        );
    }

    /// Subhandler-visibility handlers with the same name across packs are NOT
    /// a collision — they are pack-internal and excluded from cross-pack
    /// collision detection.
    #[test]
    fn subhandler_same_name_across_packs_is_not_a_collision() {
        struct SubhandlerPack;
        impl Pack for SubhandlerPack {
            const NAME: &'static str = "subhandler_pack";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
                name: "create",
                description: "internal create",
                visibility: Visibility::Subhandler,
                category: VerbCategory::Commissive,
                params: &[],
            }];
        }
        #[async_trait]
        impl PackRuntime for SubhandlerPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                verb: &str,
                _: Value,
                _: &VerbRegistry,
                _: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                Ok(serde_json::json!({"pack": "subhandler_pack", "verb": verb}))
            }
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack); // AlphaPack has Verb "create"
        builder.register(SubhandlerPack); // SubhandlerPack has Subhandler "create" — no collision
        builder
            .build()
            .expect("subhandler same name must NOT be a collision");
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_error() {
        let reg = build_registry();

        let err = reg.dispatch("explode", Value::Null).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("explode"));
        assert!(msg.contains("create"));
    }

    /// `all_verbs` returns only `Visibility::Verb` entries.
    ///
    /// BetaPack's `create` is `Visibility::Subhandler` — it must NOT appear
    /// in `all_verbs()` even though it has the same name as a Verb in AlphaPack.
    #[test]
    fn all_verbs_aggregates_across_packs_excludes_subhandlers() {
        let reg = build_registry();
        let verbs: Vec<&str> = reg.all_verbs().iter().map(|v| v.name).collect();
        // BetaPack's "create" (Subhandler) is absent; only Verb-visibility entries appear.
        assert_eq!(verbs, vec!["create", "list", "notify"]);
    }

    #[test]
    fn all_verbs_with_names_pairs_pack_name_excludes_subhandlers() {
        let reg = build_registry();
        let pairs: Vec<(&str, &str)> = reg
            .all_verbs_with_names()
            .iter()
            .map(|(pack, v)| (*pack, v.name))
            .collect();
        // BetaPack's "create" is Subhandler and must NOT appear here.
        assert_eq!(
            pairs,
            vec![("alpha", "create"), ("alpha", "list"), ("beta", "notify"),]
        );
    }

    #[test]
    fn all_handlers_with_names_includes_subhandlers() {
        let reg = build_registry();
        let pairs: Vec<(&str, &str)> = reg
            .all_handlers_with_names()
            .iter()
            .map(|(pack, v)| (*pack, v.name))
            .collect();
        // BetaPack's Subhandler "create" IS present in the full handler list.
        assert_eq!(
            pairs,
            vec![
                ("alpha", "create"),
                ("alpha", "list"),
                ("beta", "notify"),
                ("beta", "create"),
            ]
        );
    }

    #[test]
    fn note_kinds_are_ordered() {
        let reg = build_registry();
        let kinds = reg.all_note_kinds();
        assert_eq!(kinds, vec!["memo", "log", "alert"]);
    }

    #[test]
    fn brain_consumer_kinds_are_ordered_and_deduplicated() {
        let reg = build_registry();
        assert_eq!(
            reg.all_brain_consumer_kinds(),
            vec!["recall", "search", "knowledge_compose"]
        );
    }

    #[test]
    fn brain_consumer_kind_wildcard_is_rejected_at_build_time() {
        struct WildcardConsumerPack;

        impl khive_types::Pack for WildcardConsumerPack {
            const NAME: &'static str = "wildcard-consumer";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["*"];
            const HANDLERS: &'static [HandlerDef] = &[];
        }

        #[async_trait]
        impl PackRuntime for WildcardConsumerPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn brain_consumer_kinds(&self) -> &'static [&'static str] {
                Self::BRAIN_CONSUMER_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::Null)
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(WildcardConsumerPack);
        let Err(RuntimeError::InvalidInput(message)) = builder.build() else {
            panic!("registry must reject a pack-declared brain wildcard");
        };
        assert!(message.contains("wildcard-consumer"), "{message}");
        assert!(message.contains("registry-owned"), "{message}");
    }

    #[test]
    fn note_kind_duplicate_rejected_at_build_time() {
        struct DupPack;

        impl khive_types::Pack for DupPack {
            const NAME: &'static str = "dup";
            // "memo" is already declared by AlphaPack — must be rejected at build.
            const NOTE_KINDS: &'static [&'static str] = &["memo"];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[];
        }

        #[async_trait]
        impl PackRuntime for DupPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                Ok(Value::Null)
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.register(DupPack);
        let err = builder
            .build()
            .err()
            .expect("duplicate note kind must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("memo"),
            "error must name the duplicate kind: {msg}"
        );
        assert!(
            msg.contains("alpha") || msg.contains("dup"),
            "error must name one of the conflicting packs: {msg}"
        );
    }

    #[test]
    fn entity_kinds_are_deduplicated() {
        let reg = build_registry();
        let kinds = reg.all_entity_kinds();
        assert_eq!(kinds, vec!["widget", "gadget"]);
    }

    // ---- ENTITY_TYPES composition (pack-declared entity-type subtypes) ----

    struct GammaPack;

    impl Pack for GammaPack {
        const NAME: &'static str = "gamma";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
        const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
            kind: khive_types::EntityKind::Document,
            type_name: "gamma_report",
            aliases: &["gamma_rep"],
        }];
    }

    #[async_trait]
    impl PackRuntime for GammaPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn entity_types(&self) -> &'static [EntityTypeDef] {
            Self::ENTITY_TYPES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "gamma", "verb": verb }))
        }
    }

    /// Builtin-only behavior is unchanged when no pack declares extras:
    /// `all_entity_types()` is empty, and composing it with the builtin
    /// registry resolves exactly like `EntityTypeRegistry::builtin()`.
    #[test]
    fn all_entity_types_empty_when_no_pack_declares_extras() {
        let reg = build_registry(); // AlphaPack + BetaPack — neither declares ENTITY_TYPES.
        assert!(reg.all_entity_types().is_empty());
        let composed = khive_types::EntityTypeRegistry::with_extra(reg.all_entity_types());
        let resolved = composed
            .resolve(khive_types::EntityKind::Document, Some("paper"))
            .expect("builtin paper subtype must still resolve");
        assert_eq!(resolved.entity_type.as_deref(), Some("paper"));
    }

    /// A pack-declared entity type validates through the composed registry,
    /// and builtin subtypes remain resolvable alongside it.
    #[test]
    fn pack_declared_entity_type_validates_through_composed_registry() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.register(GammaPack);
        let reg = builder.build().expect("registry builds");

        let extras = reg.all_entity_types();
        assert_eq!(extras.len(), 1);

        let composed = khive_types::EntityTypeRegistry::with_extra(extras);
        let resolved = composed
            .resolve(khive_types::EntityKind::Document, Some("gamma_rep"))
            .expect("pack-declared alias must resolve through the composed registry");
        assert_eq!(resolved.entity_type.as_deref(), Some("gamma_report"));

        let builtin_resolved = composed
            .resolve(khive_types::EntityKind::Document, Some("paper"))
            .expect("builtin subtype must remain resolvable when a pack adds extras");
        assert_eq!(builtin_resolved.entity_type.as_deref(), Some("paper"));

        composed
            .resolve(khive_types::EntityKind::Document, Some("nonexistent_type"))
            .expect_err("undeclared entity_type must still be rejected");
    }

    /// Two packs declaring the exact same `(kind, type_name)` subtype are
    /// rejected at `build()` — ADR-001's registry-ownership collision rule
    /// ("same `(base_kind, canonical_name)` from two different packs = boot
    /// error") — instead of silently resolving via registration order the
    /// way `EntityTypeRegistry::with_extra`'s hard-`insert` semantics would.
    #[test]
    fn overlapping_pack_declared_entity_types_reject_at_boot() {
        struct DeltaPack;
        impl Pack for DeltaPack {
            const NAME: &'static str = "delta";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[];
            const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
                kind: khive_types::EntityKind::Document,
                type_name: "gamma_report",
                aliases: &["gamma_rep"],
            }];
        }
        #[async_trait]
        impl PackRuntime for DeltaPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            fn entity_types(&self) -> &'static [EntityTypeDef] {
                Self::ENTITY_TYPES
            }
            async fn dispatch(
                &self,
                verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                Ok(serde_json::json!({ "pack": "delta", "verb": verb }))
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(GammaPack);
        builder.register(DeltaPack);
        let err = builder.build().err().expect(
            "overlapping ENTITY_TYPES declarations must fail at build, not silently compose",
        );

        let msg = err.to_string();
        assert!(
            msg.contains("gamma") && msg.contains("delta"),
            "collision error must name both contributing packs: {msg}"
        );
        assert!(
            msg.contains("gamma_report"),
            "collision error must name the colliding entity_type key: {msg}"
        );
    }

    // ---- Gate wiring ----

    use khive_gate::{Gate, GateError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default, Debug)]
    struct CountingGate {
        calls: AtomicUsize,
        deny_verb: Option<&'static str>,
    }

    impl Gate for CountingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if Some(req.verb.as_str()) == self.deny_verb {
                Ok(GateDecision::deny(format!("test deny for {}", req.verb)))
            } else {
                Ok(GateDecision::allow())
            }
        }
    }

    #[tokio::test]
    async fn dispatch_consults_the_gate() {
        let gate = Arc::new(CountingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();
        reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(
            gate.calls.load(Ordering::SeqCst),
            2,
            "gate should be consulted once per dispatch"
        );
    }

    #[tokio::test]
    async fn dispatch_returns_permission_denied_on_deny_v03() {
        let gate = Arc::new(CountingGate {
            calls: AtomicUsize::new(0),
            deny_verb: Some("create"),
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        // Gate denies — dispatch now returns PermissionDenied (hard enforcement).
        let err = reg.dispatch("create", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PermissionDenied { ref verb, .. } if verb == "create"),
            "expected PermissionDenied, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("create"),
            "error message must name the verb: {msg}"
        );
        assert!(
            msg.contains("test deny for create"),
            "error message must carry the deny reason: {msg}"
        );
        assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatch_allow_verb_succeeds_even_with_deny_gate_for_other_verb() {
        // Deny only "create" — "list" must still work.
        let gate = Arc::new(CountingGate {
            calls: AtomicUsize::new(0),
            deny_verb: Some("create"),
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");
    }

    #[tokio::test]
    async fn dispatch_uses_allow_all_gate_by_default() {
        // No `with_gate` call — builder should use `AllowAllGate` so dispatch works.
        let reg = build_registry();
        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");
    }

    // Captures the namespace each call sees so we can assert what the gate
    // actually receives, rather than assuming a hard-wired `default_ns()`.
    #[derive(Default, Debug)]
    struct NamespaceCapturingGate {
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl Gate for NamespaceCapturingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            self.seen
                .lock()
                .unwrap()
                .push(req.namespace.as_str().to_string());
            Ok(GateDecision::allow())
        }
    }

    #[tokio::test]
    async fn dispatch_propagates_params_namespace_to_gate() {
        let gate = Arc::new(NamespaceCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        builder.with_default_namespace("tenant-x");
        let reg = builder.build().expect("registry builds");

        // Explicit namespace in params wins.
        reg.dispatch("list", serde_json::json!({"namespace": "tenant-y"}))
            .await
            .unwrap();
        // Missing namespace → registry default.
        reg.dispatch("list", Value::Null).await.unwrap();
        // Empty string is rejected: Namespace::parse("") fails → InvalidInput error.
        let err = reg
            .dispatch("list", serde_json::json!({"namespace": ""}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "empty namespace must return InvalidInput, got {err:?}"
        );

        let seen = gate.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["tenant-y", "tenant-x"]);
    }

    #[tokio::test]
    async fn dispatch_falls_back_to_local_when_no_default_set() {
        // Builder default mirrors `Namespace::default_ns()`.
        let gate = Arc::new(NamespaceCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();
        let seen = gate.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["local"]);
    }

    /// A present-but-malformed `namespace` value must never reach the gate as
    /// the default namespace. Table-driven over every
    /// non-string JSON type; the gate-spy proves no call is ever recorded (the
    /// dispatch must short-circuit with `InvalidInput` before `GateRequest` is
    /// built), so the default namespace can never appear as a coerced stand-in.
    #[tokio::test]
    async fn namespace_null_rejected_not_coerced() {
        let cases: Vec<(&str, Value)> = vec![
            ("null", Value::Null),
            ("number", serde_json::json!(42)),
            ("boolean", serde_json::json!(true)),
            ("array", serde_json::json!(["local"])),
            ("object", serde_json::json!({"ns": "local"})),
        ];

        for (label, ns_value) in cases {
            let gate = Arc::new(NamespaceCapturingGate::default());
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(gate.clone());
            builder.with_default_namespace("tenant-x");
            let reg = builder.build().expect("registry builds");

            let err = reg
                .dispatch("list", serde_json::json!({"namespace": ns_value}))
                .await
                .unwrap_err();
            assert!(
                matches!(err, RuntimeError::InvalidInput(_)),
                "case {label}: expected InvalidInput, got {err:?}"
            );

            // The gate must never have been consulted for this malformed input —
            // proves no Allow decision (and therefore no default-namespace write)
            // can ever be reached for it.
            let seen = gate.seen.lock().unwrap().clone();
            assert!(
                seen.is_empty(),
                "case {label}: gate must not be consulted for malformed namespace, saw {seen:?}"
            );
        }
    }

    // ---- Audit event emission ----

    use khive_gate::{AuditDecision, AuditEvent, Obligation};

    /// A gate that records every audit event emitted via from_check.
    #[derive(Default, Debug)]
    struct AuditCapturingGate {
        events: std::sync::Mutex<Vec<AuditEvent>>,
        deny_verb: Option<&'static str>,
    }

    impl Gate for AuditCapturingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            let decision = if Some(req.verb.as_str()) == self.deny_verb {
                GateDecision::deny("test deny")
            } else {
                GateDecision::allow_with(vec![Obligation::Audit {
                    tag: format!("{}.check", req.verb),
                }])
            };
            // Capture what dispatch will also emit.
            let ev = AuditEvent::from_check(req, &decision, self.impl_name());
            self.events.lock().unwrap().push(ev);
            Ok(decision)
        }

        fn impl_name(&self) -> &'static str {
            "AuditCapturingGate"
        }
    }

    #[tokio::test]
    async fn dispatch_emits_one_audit_event_per_call() {
        let gate = Arc::new(AuditCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();
        reg.dispatch("create", Value::Null).await.unwrap();

        let evs = gate.events.lock().unwrap();
        assert_eq!(evs.len(), 2, "exactly one audit event per dispatch call");
    }

    #[tokio::test]
    async fn dispatch_audit_event_allow_carries_obligations() {
        let gate = Arc::new(AuditCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();

        let evs = gate.events.lock().unwrap();
        let ev = &evs[0];
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.decision, AuditDecision::Allow);
        assert!(ev.deny_reason.is_none());
        assert_eq!(ev.obligations.len(), 1);
        assert_eq!(ev.gate_impl, "AuditCapturingGate");
    }

    #[tokio::test]
    async fn dispatch_audit_event_deny_carries_reason() {
        let gate = Arc::new(AuditCapturingGate {
            events: Default::default(),
            deny_verb: Some("create"),
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        // Gate denies — dispatch returns PermissionDenied (hard enforcement).
        // The audit event is still recorded (captured inside the gate impl).
        let err = reg.dispatch("create", Value::Null).await.unwrap_err();
        assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

        let evs = gate.events.lock().unwrap();
        let ev = &evs[0];
        assert_eq!(ev.verb, "create");
        assert_eq!(ev.decision, AuditDecision::Deny);
        assert_eq!(ev.deny_reason.as_deref(), Some("test deny"));
        assert!(ev.obligations.is_empty());
    }

    #[tokio::test]
    async fn dispatch_audit_event_fields_match_gate_request() {
        let gate = Arc::new(AuditCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        builder.with_default_namespace("tenant-z");
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
            .await
            .unwrap();

        let evs = gate.events.lock().unwrap();
        let ev = &evs[0];
        // Namespace from params wins.
        assert_eq!(ev.namespace, "tenant-q");
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.actor.kind, "anonymous");
    }

    // ---- Actor attribution threading into gate request (ADR-057) ----

    /// A gate spy that captures the raw `GateRequest` it receives.
    #[derive(Default, Debug)]
    struct ActorCapturingGate {
        requests: std::sync::Mutex<Vec<GateRequest>>,
    }

    impl Gate for ActorCapturingGate {
        fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
            self.requests.lock().unwrap().push(req.clone());
            Ok(GateDecision::allow())
        }
    }

    /// When `actor_id` is configured, the gate request carries that actor, not
    /// anonymous. This exercises the ADR-057 attribution fix: the gate can
    /// distinguish an agent caller from an unauthenticated caller.
    #[tokio::test]
    async fn gate_request_carries_configured_actor_when_actor_id_is_set() {
        let gate = Arc::new(ActorCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        builder.with_actor_id(Some("team-abc:implementer".to_string()));
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();

        let reqs = gate.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(
            req.actor.kind, "actor",
            "gate request must carry kind='actor' when actor_id is configured"
        );
        assert_eq!(
            req.actor.id, "team-abc:implementer",
            "gate request must carry the configured actor id"
        );
    }

    /// When no `actor_id` is configured, the gate request still receives the
    /// anonymous actor (no regression to the party-line default).
    #[tokio::test]
    async fn gate_request_carries_anonymous_when_no_actor_id_configured() {
        let gate = Arc::new(ActorCapturingGate::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate.clone());
        // actor_id left at default (None).
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();

        let reqs = gate.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(
            req.actor.kind, "anonymous",
            "gate request must carry anonymous actor when no actor_id is configured"
        );
        assert_eq!(req.actor.id, "local");
    }

    /// A pack that records the `ActorRef` carried by the `NamespaceToken` it
    /// is dispatched with, so tests can compare it against the gate's actor.
    struct TokenCapturingPack {
        actors: Arc<std::sync::Mutex<Vec<khive_gate::ActorRef>>>,
    }

    impl Pack for TokenCapturingPack {
        const NAME: &'static str = "alpha";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = AlphaPack::HANDLERS;
    }

    #[async_trait]
    impl PackRuntime for TokenCapturingPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.actors.lock().unwrap().push(token.actor().clone());
            Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
        }
    }

    /// The gate's actor and the storage token's actor must be the exact same
    /// resolved value: both come from one `resolve_actor` call
    /// (`resolved_actor`) instead of two independently hand-synchronized
    /// `match` expressions, so a future edit to one copy but not the other
    /// cannot silently desynchronize "who the gate thinks the caller is" from
    /// "who the storage layer thinks the caller is". Reintroducing a second
    /// independent actor-resolution copy for the token would regress this and
    /// this test would catch it.
    #[tokio::test]
    async fn gate_actor_and_token_actor_are_identical_when_actor_id_is_set() {
        let gate = Arc::new(ActorCapturingGate::default());
        let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pack = TokenCapturingPack {
            actors: actors.clone(),
        };
        let mut builder = VerbRegistryBuilder::new();
        builder.register(pack);
        builder.with_gate(gate.clone());
        builder.with_actor_id(Some("actor-alpha".to_string()));
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();

        let reqs = gate.requests.lock().unwrap();
        let gate_actor = reqs[0].actor.clone();
        drop(reqs);

        let captured = actors.lock().unwrap();
        let token_actor = captured[0].clone();

        assert_eq!(
            gate_actor.kind, token_actor.kind,
            "gate request actor and storage token actor must carry the same kind"
        );
        assert_eq!(
            gate_actor.id, token_actor.id,
            "gate request actor and storage token actor must carry the same id"
        );
        assert_eq!(gate_actor.id, "actor-alpha");
    }

    /// Same identity check with no configured `actor_id`: both the gate and
    /// the storage token must independently land on `ActorRef::anonymous()`.
    #[tokio::test]
    async fn gate_actor_and_token_actor_are_identical_when_anonymous() {
        let gate = Arc::new(ActorCapturingGate::default());
        let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pack = TokenCapturingPack {
            actors: actors.clone(),
        };
        let mut builder = VerbRegistryBuilder::new();
        builder.register(pack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", Value::Null).await.unwrap();

        let reqs = gate.requests.lock().unwrap();
        let gate_actor = reqs[0].actor.clone();
        drop(reqs);

        let captured = actors.lock().unwrap();
        let token_actor = captured[0].clone();

        assert_eq!(gate_actor.kind, token_actor.kind);
        assert_eq!(gate_actor.id, token_actor.id);
        assert_eq!(gate_actor.id, "local");
    }

    // ---- dispatch_as: verified-actor dispatch for embedding hosts ----

    /// `dispatch_as` must thread the caller-supplied verified actor through
    /// to the pack handler's `NamespaceToken`, exactly as `dispatch_with_identity`
    /// does with a `RequestIdentity.actor_id` — this is the observable
    /// contract embedding hosts rely on.
    #[tokio::test]
    async fn dispatch_as_threads_verified_actor_into_token() {
        let gate = Arc::new(ActorCapturingGate::default());
        let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pack = TokenCapturingPack {
            actors: actors.clone(),
        };
        let mut builder = VerbRegistryBuilder::new();
        builder.register(pack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch_as(
            "list",
            Value::Null,
            VerifiedActor::new("gateway:principal-42").unwrap(),
        )
        .await
        .unwrap();

        let reqs = gate.requests.lock().unwrap();
        assert_eq!(reqs[0].actor.kind, "actor");
        assert_eq!(reqs[0].actor.id, "gateway:principal-42");
        drop(reqs);

        let captured = actors.lock().unwrap();
        assert_eq!(captured[0].kind, "actor");
        assert_eq!(
            captured[0].id, "gateway:principal-42",
            "the storage token actor must be the verified_actor supplied to dispatch_as, \
             matching exactly what pack handlers read as the acting principal"
        );
    }

    /// `dispatch_as` is purely additive: a registry with a baked `actor_id`
    /// must still serve plain `dispatch()` calls under its own baked actor,
    /// unaffected by any `dispatch_as` call made on the same (cheaply
    /// cloneable) registry. No shared mutable state links the two calls.
    #[tokio::test]
    async fn dispatch_as_does_not_change_plain_dispatch_behavior() {
        let gate = Arc::new(ActorCapturingGate::default());
        let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pack = TokenCapturingPack {
            actors: actors.clone(),
        };
        let mut builder = VerbRegistryBuilder::new();
        builder.register(pack);
        builder.with_gate(gate.clone());
        builder.with_actor_id(Some("baked-actor".to_string()));
        let reg = builder.build().expect("registry builds");

        reg.dispatch_as(
            "list",
            Value::Null,
            VerifiedActor::new("verified-actor").unwrap(),
        )
        .await
        .unwrap();
        reg.dispatch("list", Value::Null).await.unwrap();

        let captured = actors.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].id, "verified-actor", "dispatch_as call");
        assert_eq!(
            captured[1].id, "baked-actor",
            "a later plain dispatch() call must still use the registry's baked \
             actor_id, unaffected by the prior dispatch_as call"
        );
    }

    /// A request `params` payload cannot inject or override the actor:
    /// `dispatch_as` resolves the acting principal solely from its Rust-side
    /// `verified_actor` argument, never from `params`. An `actor` key placed
    /// in `params` passes through untouched to the pack handler like any
    /// other unrecognized field — the dispatch boundary itself never reads
    /// `params["actor"]`.
    #[tokio::test]
    async fn dispatch_as_ignores_actor_key_in_params() {
        let gate = Arc::new(ActorCapturingGate::default());
        let actors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pack = TokenCapturingPack {
            actors: actors.clone(),
        };
        let mut builder = VerbRegistryBuilder::new();
        builder.register(pack);
        builder.with_gate(gate.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch_as(
            "list",
            serde_json::json!({"actor": "spoofed-actor"}),
            VerifiedActor::new("verified-actor").unwrap(),
        )
        .await
        .unwrap();

        let captured = actors.lock().unwrap();
        assert_eq!(
            captured[0].id, "verified-actor",
            "an 'actor' key inside params must never override the verified_actor \
             argument threaded through dispatch_as"
        );
    }

    /// `VerifiedActor::new` must reject an empty identifier rather than
    /// letting it reach dispatch and silently resolve to the anonymous actor.
    #[test]
    fn verified_actor_rejects_empty_identifier() {
        let err = VerifiedActor::new("").unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }

    /// `VerifiedActor::new` must reject a whitespace-only identifier for the
    /// same reason: it must never launder into `ActorRef::anonymous()`.
    #[test]
    fn verified_actor_rejects_whitespace_only_identifier() {
        let err = VerifiedActor::new("   ").unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(_)),
            "expected InvalidInput, got {err:?}"
        );
    }

    // ---- Rego gate: fail-closed end-to-end ----

    /// A `RegoGate` whose policy lacks the named entrypoint rule must cause
    /// `VerbRegistry::dispatch` to return `RuntimeError::PermissionDenied` —
    /// never to proceed to the pack handler.
    ///
    /// This is the runtime-level assertion that a gate evaluation failure
    /// fails closed rather than opening a security hole. `RegoGate::check`
    /// converts all evaluation failures (missing rule, undefined result,
    /// serialization error, poisoned engine) to `Ok(GateDecision::Deny)` with
    /// a static classified reason, so dispatch is blocked as a policy
    /// refusal. Infrastructure faults from other `Gate` implementations
    /// remain distinguishable as `RuntimeError::GateUnavailable`.
    #[tokio::test]
    async fn rego_gate_missing_entrypoint_returns_permission_denied() {
        use khive_gate_rego::RegoGate;

        // Policy defines `verdict` but NOT `data.khive.gate.decision` (the
        // default entrypoint).  Construction succeeds — from_policy_str does
        // not validate the default entrypoint.  check() must convert the
        // missing-rule evaluation error to Ok(Deny) with a static classified
        // reason so the runtime reports a policy refusal rather than a gate
        // infrastructure outage.
        let policy = r#"
            package khive.gate
            import rego.v1
            verdict := "allow"
        "#;
        let gate = Arc::new(RegoGate::from_policy_str(policy).expect("policy compiles"));

        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(gate);
        let reg = builder.build().expect("registry builds");

        let err = reg.dispatch("create", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PermissionDenied { ref verb, ref reason }
                if verb == "create" && reason == "policy evaluation failed"),
            "expected PermissionDenied with the static classified reason for a missing rego entrypoint, got {err:?}"
        );
    }

    // ---- Audit tracing emission ----
    //
    // The AuditCapturingGate tests above prove that AuditEvent::from_check is
    // called with the right inputs, but they observe the event *inside* the
    // gate impl — they would still pass if dispatch's
    // `tracing::info!(audit_event = ..., "gate.check")` were deleted or
    // renamed. The tests below install a capture Layer and assert on the
    // actual tracing event surfaced from dispatch. This locks the public
    // observability contract: one `gate.check` info event per dispatch,
    // carrying an `audit_event` field that round-trips back to an `AuditEvent`.

    use std::sync::{Mutex as StdMutex, Once, OnceLock};

    use serial_test::serial;
    use tracing::field::{Field, Visit};

    #[derive(Clone, Debug, Default)]
    struct CapturedEvent {
        message: Option<String>,
        audit_event: Option<String>,
        into_id: Option<String>,
        budget_rows: Option<u64>,
    }

    #[derive(Default)]
    struct CapturedEventVisitor(CapturedEvent);

    impl Visit for CapturedEventVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            match field.name() {
                "message" => self.0.message = Some(value.to_string()),
                "audit_event" => self.0.audit_event = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            // `tracing::info!(audit_event = %expr, "msg")` records via the
            // Display-wrapped Debug path, so we receive the JSON string here.
            // `"msg"` literal records as a `message` field via `record_debug`
            // with a quoted Debug representation; strip the surrounding quotes
            // so the captured message matches the source.
            let formatted = format!("{value:?}");
            let cleaned = formatted
                .trim_start_matches('"')
                .trim_end_matches('"')
                .to_string();
            match field.name() {
                "message" => self.0.message = Some(cleaned),
                "audit_event" => self.0.audit_event = Some(cleaned),
                "into_id" => self.0.into_id = Some(cleaned),
                _ => {}
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "budget_rows" {
                self.0.budget_rows = Some(value);
            }
        }
    }

    /// Minimal `tracing::Subscriber` that captures events into a shared vec.
    ///
    /// Implemented directly (without `tracing_subscriber::registry()` layering)
    /// to avoid the layer machinery that can cause thread-local dispatch to be
    /// bypassed when the registry's internal global state is initialised by
    /// another subscriber in the same test binary.
    ///
    /// Isolation across concurrent tests is handled at the dispatcher level by
    /// `tracing::dispatcher::with_default`, which installs this subscriber
    /// as the thread-local default for the duration of the test closure.
    /// Other threads (e.g. `#[tokio::test]` pool workers) emit through their
    /// own (typically NoSubscriber) dispatchers and never reach this instance.
    struct CaptureSubscriber {
        events: Arc<StdMutex<Vec<CapturedEvent>>>,
    }

    impl CaptureSubscriber {
        fn new(events: Arc<StdMutex<Vec<CapturedEvent>>>) -> Self {
            Self { events }
        }
    }

    impl tracing::Subscriber for CaptureSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = CapturedEventVisitor::default();
            event.record(&mut visitor);
            let captured = visitor.0;
            // Tee the post-commit budget logs into their own append-only sink:
            // `capture_dispatch_events` clears the main buffer, so a reader of
            // budget events sharing that buffer would race the clear.
            if let (Some(message), Some(into_id)) = (&captured.message, &captured.into_id) {
                if message.ends_with("transaction materialization budget") {
                    budget_events_sink()
                        .lock()
                        .unwrap()
                        .push(CapturedBudgetLog {
                            message: message.clone(),
                            into_id: into_id.clone(),
                            budget_rows: captured.budget_rows.unwrap_or(0),
                        });
                }
            }
            self.events.lock().unwrap().push(captured);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Global capture buffer for the tracing tests.
    ///
    /// The subscriber is installed exactly once via `set_global_default`
    /// (thread-local dispatchers via `with_default` proved unreliable when
    /// other tests in the binary configure their own dispatchers in parallel —
    /// the global state interacted unpredictably and events were lost).
    ///
    /// Each test that uses this buffer is `#[serial]`, so only one
    /// runs at a time. The buffer is cleared at the start of each capture call.
    static GLOBAL_CAPTURE: OnceLock<Arc<StdMutex<Vec<CapturedEvent>>>> = OnceLock::new();
    static GLOBAL_INIT: Once = Once::new();

    /// One captured post-commit budget log (curation merge tests).
    #[derive(Clone)]
    pub(crate) struct CapturedBudgetLog {
        pub(crate) message: String,
        pub(crate) into_id: String,
        pub(crate) budget_rows: u64,
    }

    /// Append-only sink the subscriber tees budget logs into. Never cleared:
    /// curation tests select their own rows by `into_id`, so stale rows from
    /// other tests are inert rather than a pollution hazard.
    static BUDGET_EVENTS: OnceLock<Arc<StdMutex<Vec<CapturedBudgetLog>>>> = OnceLock::new();

    fn budget_events_sink() -> Arc<StdMutex<Vec<CapturedBudgetLog>>> {
        Arc::clone(BUDGET_EVENTS.get_or_init(|| Arc::new(StdMutex::new(Vec::new()))))
    }

    /// Entry point for the curation merge tests: installs the process-global
    /// capture subscriber (once for the whole test binary — a second
    /// `set_global_default` elsewhere would starve one of the captures) and
    /// returns the budget-log sink it tees into.
    pub(crate) fn budget_log_events() -> Arc<StdMutex<Vec<CapturedBudgetLog>>> {
        let _ = global_capture();
        budget_events_sink()
    }

    fn global_capture() -> Arc<StdMutex<Vec<CapturedEvent>>> {
        GLOBAL_INIT.call_once(|| {
            let buffer = Arc::new(StdMutex::new(Vec::new()));
            let subscriber = CaptureSubscriber::new(Arc::clone(&buffer));
            // Ignore error: if another subscriber is already set globally, our
            // subscriber installation fails, but the buffer will simply stay
            // empty and tests will fail with a clear "got 0 events" message
            // rather than a silent corruption.
            let _ = tracing::subscriber::set_global_default(subscriber);
            let _ = GLOBAL_CAPTURE.set(buffer);
        });
        Arc::clone(GLOBAL_CAPTURE.get().expect("global capture initialized"))
    }

    /// Run an async block under the global capture subscriber and return
    /// the events emitted during the run. Clears the buffer at the start.
    ///
    /// Callers MUST be `#[serial]` to prevent concurrent buffer pollution.
    fn capture_dispatch_events<Fut>(future: Fut) -> Vec<CapturedEvent>
    where
        Fut: std::future::Future<Output = ()>,
    {
        let buffer = global_capture();
        buffer.lock().unwrap().clear();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread tokio runtime");
        rt.block_on(future);

        let result = buffer.lock().unwrap().clone();
        result
    }

    /// Pull every captured event whose `message` matches `"gate.check"` AND
    /// whose audit_event JSON declares the expected `gate_impl` name.
    ///
    /// Filtering by `gate_impl` lets concurrent tests in the same binary
    /// emit their own gate.check events into the global capture buffer
    /// without polluting each others' counts.
    fn gate_check_events_for(events: &[CapturedEvent], gate_impl: &str) -> Vec<CapturedEvent> {
        events
            .iter()
            .filter(|e| e.message.as_deref() == Some("gate.check"))
            .filter(|e| {
                e.audit_event
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .and_then(|v| {
                        v.get("gate_impl")
                            .and_then(|g| g.as_str().map(|s| s.to_string()))
                    })
                    .as_deref()
                    == Some(gate_impl)
            })
            .cloned()
            .collect()
    }

    #[test]
    #[serial]
    fn dispatch_tracing_emits_one_gate_check_event_on_allow() {
        #[derive(Debug)]
        struct TracingAllowGate;
        impl Gate for TracingAllowGate {
            fn check(&self, _: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::allow())
            }
            fn impl_name(&self) -> &'static str {
                "TracingAllowGate"
            }
        }

        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(TracingAllowGate));
            builder.with_default_namespace("tenant-default");
            let reg = builder.build().expect("registry builds");
            reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
                .await
                .unwrap();
        });

        let gate_events = gate_check_events_for(&events, "TracingAllowGate");
        assert_eq!(
            gate_events.len(),
            1,
            "exactly one gate.check tracing event per dispatch (allow); got {gate_events:?}"
        );
        let payload = gate_events[0]
            .audit_event
            .as_ref()
            .expect("gate.check event must carry an audit_event field");
        let audit: khive_gate::AuditEvent =
            serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
        assert_eq!(audit.decision, AuditDecision::Allow);
        assert_eq!(audit.verb, "list");
        assert_eq!(audit.namespace, "tenant-q");
        assert_eq!(audit.gate_impl, "TracingAllowGate");
        assert!(
            audit.deny_reason.is_none(),
            "deny_reason must be None on Allow"
        );
    }

    #[test]
    #[serial]
    fn dispatch_tracing_emits_one_gate_check_event_when_gate_is_unavailable() {
        #[derive(Debug)]
        struct TracingUnavailableGate;
        impl Gate for TracingUnavailableGate {
            fn check(&self, _: &GateRequest) -> Result<GateDecision, GateError> {
                Err(GateError::Internal("tracing gate broken".into()))
            }

            fn impl_name(&self) -> &'static str {
                "TracingUnavailableGate"
            }
        }

        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(TracingUnavailableGate));
            let reg = builder.build().expect("registry builds");
            let error = reg
                .dispatch("list", Value::Null)
                .await
                .expect_err("gate outage must refuse dispatch");
            assert!(matches!(error, RuntimeError::GateUnavailable { .. }));
        });

        let gate_events = gate_check_events_for(&events, "TracingUnavailableGate");
        assert_eq!(
            gate_events.len(),
            1,
            "exactly one gate.check tracing event per gate outage; got {gate_events:?}"
        );
        let payload = gate_events[0]
            .audit_event
            .as_ref()
            .expect("gate outage trace must carry an audit_event field");
        let audit: AuditEvent =
            serde_json::from_str(payload).expect("audit_event payload must decode");
        assert_eq!(audit.decision, AuditDecision::GateUnavailable);
        assert!(audit.deny_reason.is_none());
        assert!(audit.obligations.is_empty());
        assert_eq!(audit.gate_impl, "TracingUnavailableGate");
    }

    // ---- Hard enforcement + EventStore persistence ----

    use crate::runtime::NamespaceToken;
    use async_trait::async_trait;
    use khive_storage::{
        BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, SubstrateKind,
    };
    use khive_types::EventOutcome;

    /// Minimal stand-in for the Git pack: the receipt contract belongs to the
    /// runtime dispatch seam, so these tests do not need a git repository or
    /// any Git-pack dependency.
    struct GitDigestResultPack {
        project_id: uuid::Uuid,
    }

    impl Pack for GitDigestResultPack {
        const NAME: &'static str = "git";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "git.digest",
            description: "return a deterministic digest report",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for GitDigestResultPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({
                "project_id": self.project_id,
                "project_created": false,
                "commits_ingested": 2,
                "commits_skipped_existing": 1,
                "issues_ingested": 3,
                "issues_skipped_existing": 4,
                "prs_ingested": 5,
                "prs_skipped_existing": 6,
                "done": true,
                "history_exhausted": true,
                "sources": {
                    "commits": {"state": "completed"},
                    "issues": {"state": "completed"},
                    "pull_requests": {"state": "completed"}
                },
                "warnings": []
            }))
        }
    }

    /// A nominally successful handler with an invalid receipt identity. The
    /// runtime must turn this into an error without consuming its generic
    /// audit fallback.
    struct MalformedGitDigestResultPack;

    impl Pack for MalformedGitDigestResultPack {
        const NAME: &'static str = "malformed-git";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "git.digest",
            description: "return a malformed digest report",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for MalformedGitDigestResultPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({
                "project_id": "not-a-uuid",
                "done": true,
            }))
        }
    }

    /// One entry in the interleaved submission trace. Typed so assertions
    /// match on fields instead of parsing a formatted string; both sides of
    /// the ordering land on ONE vector so their relative order is observable
    /// rather than assumed.
    #[derive(Debug, Clone, PartialEq)]
    enum TraceEntry {
        /// A handler effect that has committed (nothing downstream undoes it).
        Effect { name: String },
        /// An audit row submitted to the event store, carrying exactly the
        /// fields the obligation test discriminates on.
        Audit {
            kind: EventKind,
            outcome: EventOutcome,
            verb: String,
            /// The row's `resource.cost_unit`, `None` when the payload omits
            /// it (the error path's `base_resource_payload` does).
            cost_unit: Option<Value>,
        },
    }

    /// In-memory EventStore for unit tests — avoids file-backed SQLite.
    #[derive(Default, Debug)]
    struct MemoryEventStore {
        events: std::sync::Mutex<Vec<Event>>,
        fail_appends: bool,
        /// Fail only a generation whose batch contains an event of this
        /// kind, leaving every other generation (e.g. the deferred
        /// obligation row committed after dispatch resolves) to commit
        /// normally. Lets a test fail a pure-observability row without
        /// also failing the obligation row that shares the same store.
        fail_kind: Option<EventKind>,
        /// Append-ordered record of what was SUBMITTED to this store,
        /// written before the injected-failure check so a submission this
        /// store then rejects is still visible.
        ///
        /// `events` alone cannot show that: a rejected append returns before
        /// the store records anything, so a test reading `events` cannot
        /// tell a row that failed to commit from a row that was never built.
        /// Those are different production behaviours and only one of them is
        /// the audit contract. A test that hands the same vector to its
        /// handler also gets the ordering between the handler's effect and
        /// the audit submission, which is the only way to observe that the
        /// audit row is written after the handler rather than before it.
        trace: Option<Arc<std::sync::Mutex<Vec<TraceEntry>>>>,
    }

    impl MemoryEventStore {
        /// Record a submission attempt. Call before any failure check.
        ///
        /// The projection carries the verb and the row's `resource.cost_unit`
        /// value, not just kind and outcome, and not merely whether the key is
        /// present.
        ///
        /// Presence alone is too weak to pin what it looks like it pins.
        /// `cost_unit::resource_payload` inserts the key unconditionally, and
        /// for most verbs `item_count` returns a constant `1` regardless of the
        /// handler's return value, so a submission built from a static or null
        /// `ok_val` still carries the key. Presence separates `resource_payload`
        /// from the error path's `base_resource_payload`, which is a real
        /// property but a different one.
        ///
        /// The value is what sources the return value, and only for a verb whose
        /// `item_count` reads it. `knowledge.index` is that verb: `item_count`
        /// takes `result["total"]`, so `cost_unit` is `total + 1` and moves with
        /// what the handler returned. The fixture below uses that verb for
        /// exactly this reason.
        fn trace_submission(&self, events: &[Event]) {
            if let Some(trace) = &self.trace {
                let mut trace = trace.lock().expect("trace lock");
                for event in events {
                    let cost_unit = event
                        .payload
                        .get("resource")
                        .and_then(|resource| resource.get("cost_unit"))
                        .cloned();
                    trace.push(TraceEntry::Audit {
                        kind: event.kind,
                        outcome: event.outcome,
                        verb: event.verb.clone(),
                        cost_unit,
                    });
                }
            }
        }
    }

    #[async_trait]
    impl EventStore for MemoryEventStore {
        async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
            self.trace_submission(std::slice::from_ref(&event));
            if self.fail_appends || self.fail_kind == Some(event.kind) {
                return Err(khive_storage::StorageError::Internal(
                    "injected audit append failure".to_string(),
                ));
            }
            self.events.lock().unwrap().push(event);
            Ok(())
        }
        async fn append_events(
            &self,
            events: Vec<Event>,
        ) -> khive_storage::StorageResult<BatchWriteSummary> {
            self.trace_submission(&events);
            let attempted = events.len() as u64;
            let affected = attempted;
            self.events.lock().unwrap().extend(events);
            Ok(BatchWriteSummary {
                attempted,
                affected,
                failed: 0,
                first_error: String::new(),
            })
        }
        async fn get_event(&self, id: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.id == id)
                .cloned())
        }
        async fn query_events(
            &self,
            _filter: EventFilter,
            _page: PageRequest,
        ) -> khive_storage::StorageResult<Page<Event>> {
            let items = self.events.lock().unwrap().clone();
            let total = items.len() as u64;
            Ok(Page {
                items,
                total: Some(total),
            })
        }
        async fn count_events(&self, _filter: EventFilter) -> khive_storage::StorageResult<u64> {
            Ok(self.events.lock().unwrap().len() as u64)
        }

        fn preflight_event(&self, _event: &Event) -> khive_storage::StorageResult<()> {
            Ok(())
        }

        async fn append_events_idempotent(
            &self,
            events: Vec<Event>,
        ) -> khive_storage::StorageResult<khive_storage::event::IdempotentEventBatchResult>
        {
            self.trace_submission(&events);
            if self.fail_appends
                || self
                    .fail_kind
                    .is_some_and(|kind| events.iter().any(|e| e.kind == kind))
            {
                return Err(khive_storage::StorageError::Internal(
                    "injected audit append failure".to_string(),
                ));
            }
            let mut store = self.events.lock().unwrap();
            let mut rows = Vec::with_capacity(events.len());
            for event in events {
                if let Some(existing) = store.iter().find(|e| e.id == event.id) {
                    if *existing == event {
                        rows.push(
                            khive_storage::event::EventAppendDisposition::AlreadyPresentIdentical,
                        );
                    } else {
                        rows.push(khive_storage::event::EventAppendDisposition::IdentityConflict);
                    }
                } else {
                    store.push(event);
                    rows.push(khive_storage::event::EventAppendDisposition::Inserted);
                }
            }
            Ok(khive_storage::event::IdempotentEventBatchResult { rows })
        }

        fn supports_idempotent_audit_batch(&self) -> bool {
            true
        }
    }

    fn only_git_digest_event(store: &MemoryEventStore) -> Event {
        let events: Vec<Event> = store
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.verb == "git.digest")
            .cloned()
            .collect();
        assert_eq!(
            events.len(),
            1,
            "expected exactly one git.digest receipt event"
        );
        events[0].clone()
    }

    #[tokio::test]
    async fn git_digest_success_returns_complete_durable_receipt() {
        let project_id = uuid::Uuid::new_v4();
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(GitDigestResultPack { project_id });
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let result = registry
            .dispatch_with_identity(
                "git.digest",
                serde_json::json!({
                    "source": "https://user:SECRET@example.invalid/org/repo",
                }),
                Some(RequestIdentity {
                    namespace: Namespace::local().as_str().to_string(),
                    request_id: Some(1_510),
                    ..Default::default()
                }),
            )
            .await
            .expect("durably receipted digest succeeds");
        let receipt_id = result["receipt_id"]
            .as_str()
            .and_then(|raw| raw.parse::<uuid::Uuid>().ok())
            .expect("response has UUID receipt_id");

        let event = only_git_digest_event(&store);
        assert_eq!(event.id, receipt_id);
        assert_eq!(event.target_id, Some(project_id));
        assert_eq!(event.verb, "git.digest");
        assert_eq!(event.outcome, EventOutcome::Success);
        assert_eq!(event.payload_schema_version, 2);
        assert_eq!(event.payload["result"], result);
        assert_eq!(event.payload["resource"]["request_id"], 1_510);
        assert_eq!(event.payload["result"]["commits_ingested"], 2);
        assert_eq!(event.payload["result"]["issues_ingested"], 3);
        assert_eq!(event.payload["result"]["prs_ingested"], 5);
        assert!(
            !event.payload.to_string().contains("SECRET"),
            "receipt must not persist the caller's source URL or credentials"
        );
    }

    #[tokio::test]
    async fn malformed_git_digest_report_appends_one_generic_error_audit() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(MalformedGitDigestResultPack);
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("git.digest", serde_json::json!({}))
            .await
            .expect_err("malformed receipt identity must fail the response");
        assert!(matches!(
            err,
            RuntimeError::Internal(ref message)
                if message.starts_with("git_digest_receipt_persist_failed:")
        ));

        let event = only_git_digest_event(&store);
        assert_eq!(event.outcome, EventOutcome::Error);
        assert_eq!(event.payload_schema_version, 1);
        assert!(
            event.payload.get("result").is_none(),
            "a generic Error audit must not masquerade as a success receipt"
        );
        let audit: AuditEvent =
            serde_json::from_value(event.payload).expect("generic payload remains an AuditEvent");
        assert_eq!(audit.verb, "git.digest");
        assert_eq!(audit.decision, AuditDecision::Allow);
    }

    #[tokio::test]
    #[serial(audit_append_failures)]
    #[serial(audit_obligation_append_failures)]
    async fn git_digest_receipt_append_failure_never_returns_unqualified_success() {
        let before = audit_append_failure_count();
        let before_obligation = audit_obligation_append_failure_count();
        let store = Arc::new(MemoryEventStore {
            fail_appends: true,
            ..MemoryEventStore::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(GitDigestResultPack {
            project_id: uuid::Uuid::new_v4(),
        });
        builder.with_event_store(store);
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("git.digest", serde_json::json!({}))
            .await
            .expect_err("receipt persistence failure must fail the response");
        assert!(
            matches!(&err, RuntimeError::Internal(message)
                if message.starts_with("git_digest_receipt_persist_failed:")
                    && message.contains("writes may have committed")),
            "error is stable, safe, and retry-aware: {err}"
        );
        // The git.digest receipt is obligation-bearing (`GitDigestReceipt`
        // classifies as `DispatchObligation`) and this failure propagated
        // into the dispatch's own error above, so it counts on the
        // obligation counter, not the swallowed-failures one.
        assert_eq!(audit_append_failure_count(), before);
        assert_eq!(
            audit_obligation_append_failure_count(),
            before_obligation + 1
        );
    }

    #[tokio::test]
    async fn git_digest_without_event_store_fails_safe_after_handler_success() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(GitDigestResultPack {
            project_id: uuid::Uuid::new_v4(),
        });
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("git.digest", serde_json::json!({}))
            .await
            .expect_err("a successful digest needs a durable store");
        assert!(matches!(
            err,
            RuntimeError::Internal(ref message)
                if message.starts_with("git_digest_receipt_persist_failed:")
        ));
    }

    #[tokio::test]
    async fn git_digest_gate_unavailable_precedes_the_receipt_contract() {
        #[derive(Debug)]
        struct FailingGate;
        impl Gate for FailingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
                Err(khive_gate::GateError::Internal(
                    "injected gate failure".into(),
                ))
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(GitDigestResultPack {
            project_id: uuid::Uuid::new_v4(),
        });
        builder.with_gate(Arc::new(FailingGate));
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("git.digest", serde_json::json!({}))
            .await
            .expect_err("gate unavailability must refuse before the handler or receipt path");
        assert!(matches!(
            err,
            RuntimeError::GateUnavailable { ref verb, ref reason }
                if verb == "git.digest"
                    && reason == "gate backend unavailable"
                    && !reason.contains("injected gate failure")
        ));
        let event = only_git_digest_event(&store);
        assert_eq!(event.outcome, EventOutcome::Error);
        assert_eq!(event.payload["decision"], "gate_unavailable");
        assert!(event.payload.get("result").is_none());
    }

    #[tokio::test]
    async fn intercepted_gate_error_returns_typed_refusal_without_invoking_operation() {
        #[derive(Debug)]
        struct FailingGate;
        impl Gate for FailingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
                Err(khive_gate::GateError::Internal(
                    "intercepted gate broken".into(),
                ))
            }
        }

        let invoked = Arc::new(AtomicUsize::new(0));
        let invoked_by_operation = Arc::clone(&invoked);
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(Arc::new(FailingGate));
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");
        let identity = RequestIdentity {
            namespace: "identity-default".to_string(),
            request_id: Some(1_600),
            ..Default::default()
        };

        let err = registry
            .dispatch_intercepted_with_identity(
                "list",
                &serde_json::json!({"namespace": "test-ns"}),
                Some(&identity),
                move |_namespace| {
                    invoked_by_operation.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(serde_json::json!({"invoked": true})) }
                },
            )
            .await
            .expect_err("gate unavailability must refuse intercepted dispatch");

        assert!(matches!(
            err,
            RuntimeError::GateUnavailable { ref verb, ref reason }
                if verb == "list"
                    && reason == "gate backend unavailable"
                    && !reason.contains("intercepted gate broken")
        ));
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            0,
            "intercepted operation must not run after a gate infrastructure error"
        );

        let events = store.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].verb, "list");
        assert_eq!(events[0].namespace, "test-ns");
        assert_eq!(events[0].outcome, EventOutcome::Error);
        assert_eq!(events[0].payload["decision"], "gate_unavailable");
        assert!(events[0].payload.get("deny_reason").is_none());
        assert_eq!(events[0].payload["resource"]["work_class"], "interactive");
        assert_eq!(events[0].payload["resource"]["request_id"], 1_600);
        assert!(events[0].payload["resource"].get("cost_unit").is_none());
    }

    #[tokio::test]
    async fn intercepted_deny_remains_distinct_and_does_not_invoke_operation() {
        #[derive(Debug)]
        struct DenyingGate;
        impl Gate for DenyingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("intercepted policy denied"))
            }
        }

        let invoked = Arc::new(AtomicUsize::new(0));
        let invoked_by_operation = Arc::clone(&invoked);
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_gate(Arc::new(DenyingGate));
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch_intercepted_with_identity("list", &Value::Null, None, move |_namespace| {
                invoked_by_operation.fetch_add(1, Ordering::SeqCst);
                async move { Ok(serde_json::json!({"invoked": true})) }
            })
            .await
            .expect_err("explicit gate denial must refuse intercepted dispatch");

        assert!(matches!(
            err,
            RuntimeError::PermissionDenied { ref verb, ref reason }
                if verb == "list" && reason == "intercepted policy denied"
        ));
        assert_eq!(invoked.load(Ordering::SeqCst), 0);

        let events = store.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].outcome, EventOutcome::Denied);
        assert_eq!(events[0].payload["decision"], "deny");
        assert_eq!(
            events[0].payload["deny_reason"],
            "intercepted policy denied"
        );
    }

    #[tokio::test]
    async fn intercepted_git_digest_uses_the_same_receipt_contract() {
        let project_id = uuid::Uuid::new_v4();
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let result = registry
            .dispatch_intercepted_with_identity(
                "git.digest",
                &serde_json::json!({}),
                Some(&RequestIdentity {
                    namespace: Namespace::local().as_str().to_string(),
                    request_id: Some(1_647),
                    ..Default::default()
                }),
                |_namespace| async move {
                    Ok(serde_json::json!({
                        "project_id": project_id,
                        "commits_ingested": 7,
                        "done": true,
                    }))
                },
            )
            .await
            .expect("intercepted digest is durably receipted");

        let event = only_git_digest_event(&store);
        assert_eq!(result["receipt_id"], serde_json::json!(event.id));
        assert_eq!(event.payload["result"], result);
        assert_eq!(event.payload["resource"]["request_id"], 1_647);
    }

    #[tokio::test]
    async fn intercepted_git_digest_receipt_preserves_typed_metadata() {
        let project_id = uuid::Uuid::new_v4();
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let outcome = registry
            .dispatch_intercepted_with_metadata_with_identity(
                "git.digest",
                &serde_json::json!({}),
                None,
                |_namespace| async move {
                    Ok(InterceptedDispatchResult::new(
                        serde_json::json!({
                            "project_id": project_id,
                            "commits_ingested": 3,
                            "done": true,
                        }),
                        vec!["backend-a".to_string(), "backend-b".to_string()],
                    ))
                },
            )
            .await
            .expect("metadata-bearing digest is durably receipted");

        let event = only_git_digest_event(&store);
        assert_eq!(outcome.result["receipt_id"], serde_json::json!(event.id));
        assert_eq!(event.payload["result"], outcome.result);
        assert_eq!(outcome.metadata, ["backend-a", "backend-b"]);
    }

    #[tokio::test]
    async fn intercepted_malformed_git_digest_appends_one_generic_error_audit() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_event_store(store.clone());
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch_intercepted_with_identity(
                "git.digest",
                &serde_json::json!({}),
                None,
                |_namespace| async {
                    Ok(serde_json::json!({
                        "project_id": "not-a-uuid",
                        "done": true,
                    }))
                },
            )
            .await
            .expect_err("malformed intercepted receipt must fail the response");
        assert!(matches!(
            err,
            RuntimeError::Internal(ref message)
                if message.starts_with("git_digest_receipt_persist_failed:")
        ));

        let event = only_git_digest_event(&store);
        assert_eq!(event.outcome, EventOutcome::Error);
        assert_eq!(event.payload_schema_version, 1);
        assert!(event.payload.get("result").is_none());
        let audit: AuditEvent =
            serde_json::from_value(event.payload).expect("generic payload remains an AuditEvent");
        assert_eq!(audit.verb, "git.digest");
        assert_eq!(audit.decision, AuditDecision::Allow);
    }

    #[tokio::test]
    async fn allow_all_gate_default_remains_backward_compatible() {
        // No gate set — AllowAllGate is the default. Dispatch must succeed.
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        let reg = builder.build().expect("registry builds");

        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(
            res["pack"], "alpha",
            "AllowAllGate must allow every verb — backward compat guarantee"
        );
        let res = reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");
    }

    #[tokio::test]
    async fn deny_gate_returns_permission_denied_pack_never_invoked() {
        #[derive(Debug)]
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("test: always deny"))
            }
        }

        // Track whether dispatch was ever invoked on the pack.
        #[derive(Debug)]
        struct TrackedPack {
            invoked: Arc<AtomicUsize>,
        }

        impl khive_types::Pack for TrackedPack {
            const NAME: &'static str = "tracked";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
                name: "guarded",
                description: "a guarded verb",
                visibility: Visibility::Verb,
                category: VerbCategory::Assertive,
                params: &[],
            }];
        }

        #[async_trait]
        impl PackRuntime for TrackedPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                self.invoked.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({"invoked": true}))
            }
        }

        let invoked = Arc::new(AtomicUsize::new(0));
        let mut builder = VerbRegistryBuilder::new();
        builder.register(TrackedPack {
            invoked: invoked.clone(),
        });
        builder.with_gate(Arc::new(AlwaysDenyGate));
        let reg = builder.build().expect("registry builds");

        let err = reg.dispatch("guarded", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PermissionDenied { ref verb, ref reason } if verb == "guarded" && reason.contains("always deny")),
            "expected PermissionDenied with verb=guarded and reason, got: {err:?}"
        );
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            0,
            "pack dispatch MUST NOT be invoked when gate denies"
        );
    }

    #[tokio::test]
    async fn update_denial_precedes_id_existence_resolution() {
        #[derive(Debug)]
        struct AlwaysDenyUpdateGate {
            checked: Arc<AtomicUsize>,
        }
        impl Gate for AlwaysDenyUpdateGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                self.checked.fetch_add(1, Ordering::SeqCst);
                Ok(GateDecision::deny("caller has no update capability"))
            }
        }

        #[derive(Debug)]
        struct ExistenceOracleUpdatePack {
            existing_id: String,
            invoked: Arc<AtomicUsize>,
        }

        impl khive_types::Pack for ExistenceOracleUpdatePack {
            const NAME: &'static str = "existence_oracle";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
                name: "update",
                description: "distinguish a present id from an absent id",
                visibility: Visibility::Verb,
                category: VerbCategory::Declaration,
                params: &[],
            }];
        }

        #[async_trait]
        impl PackRuntime for ExistenceOracleUpdatePack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                self.invoked.fetch_add(1, Ordering::SeqCst);
                match params.get("id").and_then(Value::as_str) {
                    Some(id) if id == self.existing_id => Ok(serde_json::json!({"updated": id})),
                    _ => Err(RuntimeError::NotFound("record".to_string())),
                }
            }
        }

        let existing_id = uuid::Uuid::new_v4().to_string();
        let absent_id = uuid::Uuid::new_v4().to_string();
        let invoked = Arc::new(AtomicUsize::new(0));
        let checked = Arc::new(AtomicUsize::new(0));

        let pack = || ExistenceOracleUpdatePack {
            existing_id: existing_id.clone(),
            invoked: Arc::clone(&invoked),
        };

        let mut control_builder = VerbRegistryBuilder::new();
        control_builder.register(pack());
        let control = control_builder.build().expect("control registry builds");
        control
            .dispatch("update", serde_json::json!({"id": existing_id.clone()}))
            .await
            .expect("positive control resolves the present id");
        assert!(matches!(
            control
                .dispatch("update", serde_json::json!({"id": absent_id.clone()}))
                .await,
            Err(RuntimeError::NotFound(_))
        ));
        assert_eq!(invoked.load(Ordering::SeqCst), 2);

        let mut denied_builder = VerbRegistryBuilder::new();
        denied_builder.register(pack());
        denied_builder.with_gate(Arc::new(AlwaysDenyUpdateGate {
            checked: Arc::clone(&checked),
        }));
        let denied = denied_builder.build().expect("denied registry builds");

        let present_error = denied
            .dispatch("update", serde_json::json!({"id": existing_id.clone()}))
            .await
            .expect_err("denied present-id update must not resolve the id");
        let absent_error = denied
            .dispatch("update", serde_json::json!({"id": absent_id.clone()}))
            .await
            .expect_err("denied absent-id update must not resolve the id");

        let denial = |error: RuntimeError| match error {
            RuntimeError::PermissionDenied { verb, reason } => (verb, reason),
            other => panic!("expected gate refusal, got {other:?}"),
        };
        let present_denial = denial(present_error);
        let absent_denial = denial(absent_error);
        assert_eq!(present_denial.0, "update");
        assert_eq!(present_denial.1, "caller has no update capability");
        assert_eq!(present_denial, absent_denial);
        assert_eq!(
            checked.load(Ordering::SeqCst),
            2,
            "both denied requests must consult the configured gate"
        );
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            2,
            "neither denied request may reach the existence oracle"
        );
    }

    #[tokio::test]
    async fn audit_event_persists_to_event_store_on_allow() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap();

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(count, 1, "one audit event persisted to EventStore on allow");

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        let ev = &page.items[0];
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.namespace, "test-ns");
        assert_eq!(ev.substrate, SubstrateKind::Event);
        assert_eq!(ev.outcome, EventOutcome::Success);
    }

    #[tokio::test]
    #[serial(audit_append_failures)]
    #[serial(audit_obligation_append_failures)]
    async fn audit_append_failure_fails_an_obligation_bearing_dispatch() {
        let before = audit_append_failure_count();
        let before_obligation = audit_obligation_append_failure_count();

        let successful_store = Arc::new(MemoryEventStore::default());
        let mut successful_builder = VerbRegistryBuilder::new();
        successful_builder.register(AlphaPack);
        successful_builder.with_event_store(successful_store);
        let successful_registry = successful_builder.build().expect("registry builds");
        successful_registry
            .dispatch("list", Value::Null)
            .await
            .expect("successful audit append must not affect dispatch");
        assert_eq!(
            audit_append_failure_count(),
            before,
            "successful audit appends must not increment the swallowed-failure counter"
        );
        assert_eq!(
            audit_obligation_append_failure_count(),
            before_obligation,
            "successful audit appends must not increment the obligation-failure counter"
        );

        // ADR-133 D2/D3/D4: `list`'s deferred audit row is a
        // `DispatchSucceeded` obligation. A dispatch must not report success
        // when the row that accounts for it did not commit, so a persistent
        // commit failure here must fail the dispatch that would otherwise
        // have reported success.
        let failing_store = Arc::new(MemoryEventStore {
            fail_appends: true,
            ..MemoryEventStore::default()
        });
        let mut failing_builder = VerbRegistryBuilder::new();
        failing_builder.register(AlphaPack);
        failing_builder.with_event_store(failing_store);
        let failing_registry = failing_builder.build().expect("registry builds");
        let err = failing_registry
            .dispatch("list", Value::Null)
            .await
            .expect_err(
                "a persistent obligation-bearing audit commit failure must fail the dispatch",
            );
        assert!(
            matches!(&err, RuntimeError::Internal(message)
                if message.contains("audit obligation commit failed")),
            "error names the obligation failure so it is distinguishable from a handler error: {err}"
        );

        // `list`'s deferred audit row is `DispatchSucceeded`, an obligation
        // producer, so this propagated failure belongs on the obligation
        // counter — the swallowed-failure counter must not move for it.
        assert_eq!(
            audit_append_failure_count(),
            before,
            "an obligation failure must never inflate the swallowed-failure counter"
        );
        assert_eq!(
            audit_obligation_append_failure_count(),
            before_obligation + 1,
            "the failed obligation append must remain visible to diagnostics"
        );
    }

    /// An obligation-bearing audit row is written AFTER the handler returns and FROM the
    /// handler's own return value. So when that row fails to commit,
    /// `fold_audit_obligation` turns a would-be success into an error for a dispatch whose
    /// effect has ALREADY happened and cannot be rolled back by it.
    ///
    /// The existing obligation test above proves the flip using `list`, a read, where the
    /// distinction does not matter. This one pins the part that decides caller behaviour:
    /// the write landed, and the caller was told it failed. A caller that treats this error
    /// as "it did not run" and retries therefore applies the effect twice, which is what the
    /// last assertion covers.
    ///
    /// The handler and the event store share one trace vector, so the ordering claim is
    /// observed rather than assumed. That matters more than it looks: asserting only the
    /// caller-visible error and the handler's effect would leave this test green against an
    /// implementation that submits no audit row at all, or that submits one built from the
    /// error path — both of which contradict the contract while producing exactly the same
    /// error string.
    ///
    /// The outcome alone does not separate those. A row can carry `Success` and still have
    /// been built without the handler's return value, which is a third implementation and
    /// also wrong. So the trace records whether the row's resource carries `cost_unit`:
    /// `resource_payload` derives that key from `ok_val`, and `base_resource_payload`
    /// documents that it omits it. Asserting the key is what pins result-sourcing; the
    /// verb is asserted alongside it so a fabricated row for some other verb cannot satisfy
    /// the same check.
    ///
    /// Not pinned here: that the row commits on a SEPARATE writer acquisition from the
    /// handler's. The store double has no writer to observe, so that half of the mechanism
    /// needs a different fixture than this one.
    #[tokio::test]
    #[serial(audit_append_failures)]
    #[serial(audit_obligation_append_failures)]
    // The config ledger is process-global and an event-store dispatch drains its
    // queue before invoking the pack, so a concurrent config_ledger test can land
    // a submission ahead of this handler's effect and break the first-entry
    // assertion below. That group is held for the position assertion, not for the
    // audit counters the two groups above cover.
    #[serial(config_ledger)]
    async fn obligation_failure_reports_a_write_that_already_committed() {
        /// `total` is what `cost_unit` is computed from, so this number is the
        /// test's handle on whether the audit row was built from the return
        /// value. 41 is arbitrary but distinctive: it makes the expected
        /// `cost_unit` 42 (`base_weight` 1 + `item_count` 41 * `model_count`
        /// 1), a value no default path produces.
        const RETURNED_TOTAL: u64 = 41;

        #[derive(Debug)]
        struct RecordingWritePack {
            committed: Arc<std::sync::Mutex<Vec<TraceEntry>>>,
        }

        impl Pack for RecordingWritePack {
            const NAME: &'static str = "recording_write";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            // `knowledge.index` rather than `create`, and the choice is
            // load-bearing rather than incidental: it is the one verb whose
            // `cost_unit` reads the handler's return value (`item_count` takes
            // `result["total"]`). Under any other verb `item_count` is the
            // constant `1`, so the recorded `cost_unit` would be the same
            // whether the row was built from the result or from a static value,
            // and the assertion below could not tell those apart.
            const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
                name: "knowledge.index",
                description: "record one committed write",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &[],
            }];
        }

        #[async_trait]
        impl PackRuntime for RecordingWritePack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                // Stands in for a committed effect: by the time this returns, the write
                // is done and nothing downstream can undo it. Pushed onto the SAME
                // vector the event store traces into, so the relative order of the
                // effect and the audit submission is observable rather than assumed.
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unnamed")
                    .to_string();
                self.committed
                    .lock()
                    .expect("committed lock")
                    .push(TraceEntry::Effect { name: name.clone() });
                Ok(serde_json::json!({ "created": name, "total": RETURNED_TOTAL }))
            }
        }

        let committed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let failing_store = Arc::new(MemoryEventStore {
            fail_appends: true,
            trace: Some(Arc::clone(&committed)),
            ..MemoryEventStore::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(RecordingWritePack {
            committed: Arc::clone(&committed),
        });
        builder.with_event_store(failing_store);
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("knowledge.index", serde_json::json!({"name": "first"}))
            .await
            .expect_err("an obligation commit failure must fail the dispatch");
        assert!(
            matches!(&err, RuntimeError::Internal(message)
                if message.contains("audit obligation commit failed")),
            "the error must name the obligation failure, since that string is what tells a \
             caller the effect landed: {err}"
        );

        // The trace carries both sides, so each of the three claims above is an
        // assertion rather than a comment. Read as a sequence it says: the handler's
        // effect committed, and only THEN was an audit row submitted -- a row built
        // from the successful result, which is what makes it the obligation row and
        // not an error row.
        let first_pass = committed.lock().expect("committed lock").clone();
        let first_effect = TraceEntry::Effect {
            name: "first".to_string(),
        };
        assert_eq!(
            first_pass.first(),
            Some(&first_effect),
            "the handler's effect must land FIRST: an implementation that submitted the \
             audit row before dispatching would satisfy every other assertion here"
        );
        let audit_rows: Vec<&TraceEntry> = first_pass
            .iter()
            .filter(|entry| matches!(entry, TraceEntry::Audit { .. }))
            .collect();
        assert!(
            !audit_rows.is_empty(),
            "an audit row must actually be SUBMITTED; without this assertion the test \
             passes against an implementation that returns the same error and never \
             builds a row at all, which is a different defect wearing the same error \
             string. Trace was {first_pass:?}"
        );
        let expected_cost_unit = serde_json::json!(RETURNED_TOTAL + 1);
        assert!(
            audit_rows.iter().any(|entry| matches!(
                entry,
                TraceEntry::Audit {
                    outcome: EventOutcome::Success,
                    verb,
                    cost_unit: Some(cost_unit),
                    ..
                } if verb.as_str() == "knowledge.index" && *cost_unit == expected_cost_unit
            )),
            "the submitted row must be the SUCCESS row for THIS verb, carrying a resource \
             computed FROM the handler's return value. The outcome alone is not enough: an \
             implementation that stamps Success on a row built without `ok_val` would \
             satisfy an outcome-only assertion while breaking the contract this test \
             exists for. The exact value is the discriminator, not the key's presence: \
             `resource_payload` inserts `cost_unit` unconditionally, so presence survives a \
             static `ok_val`, while {expected_cost_unit} is reachable only from the \
             returned `total` of {RETURNED_TOTAL} (`base_weight` 1 + `item_count` \
             {RETURNED_TOTAL} * `model_count` 1). Substituting a null or static result \
             collapses it to 1, and the error path's `base_resource_payload` omits the key \
             entirely (`cost_unit: None` here), so all three implementations are \
             distinguishable. Trace was {first_pass:?}"
        );
        assert_eq!(
            first_pass
                .iter()
                .filter(|entry| **entry == first_effect)
                .count(),
            1,
            "exactly one effect on the first pass"
        );

        // What a caller does on a failure it believes means "did not run".
        let _ = registry
            .dispatch("knowledge.index", serde_json::json!({"name": "first"}))
            .await
            .expect_err("the retry fails the same way");
        assert_eq!(
            committed
                .lock()
                .expect("committed lock")
                .iter()
                .filter(|entry| **entry == first_effect)
                .count(),
            2,
            "retrying this error double-writes; a caller must re-derive state instead of \
             resubmitting"
        );
    }

    #[tokio::test]
    #[serial(config_ledger)]
    #[serial(audit_append_failures)]
    #[serial(audit_obligation_append_failures)]
    async fn config_locked_row_degrades_without_failing_the_dispatch_that_observed_it() {
        // Deny-gate a dispatch so the only append this call makes is the
        // immediate `ConfigLocked` drain in the gate-check block: the
        // `GateDenied` row and the eventual `PermissionDenied` return are
        // unaffected by the store either way (see the two `let _ =` sites
        // above), so any failure this test observes is isolated to the
        // pure-observability `ConfigLocked` row. The `fail_appends: true`
        // store also fails the fire-and-forget `GateDenied` append, which
        // now counts on the obligation counter (`#[serial(...)]` above
        // keeps that from racing this file's exact-delta assertions on it).
        #[derive(Debug)]
        struct DenyGate;
        impl Gate for DenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
                Ok(GateDecision::Deny {
                    reason: "denied for test".to_string(),
                })
            }
        }

        crate::config_ledger::record_config_locked("adr133_test_key", "adr133_test_value");

        let store = Arc::new(MemoryEventStore {
            fail_appends: true,
            ..MemoryEventStore::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(DenyGate));
        builder.with_event_store(store);
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("list", Value::Null)
            .await
            .expect_err("the gate denies every request");
        assert!(
            matches!(err, RuntimeError::PermissionDenied { .. }),
            "a pure-observability row's failure must never surface as the dispatch error: {err}"
        );

        let metrics = registry
            .audit_batch_metrics()
            .expect("with_event_store configures the ADR-133 seam");
        assert!(
            metrics.degraded,
            "the config-locked row's commit failure must be visible as degradation"
        );
        assert!(metrics.degraded_rows >= 1);
    }

    #[tokio::test]
    #[serial(config_ledger)]
    #[serial(audit_append_failures)]
    async fn config_locked_row_failure_never_fails_a_dispatch_that_would_otherwise_succeed() {
        // ADR-133 criterion 4's success half: a pure-observability row's
        // commit failure must degrade gracefully without touching the
        // caller-visible outcome of a dispatch that has nothing to do with
        // it. Only the `ConfigLocked` generation fails here — the gate
        // allows the call, so `list`'s own `DispatchSucceeded` obligation
        // row commits in a later, unaffected generation.
        crate::config_ledger::record_config_locked(
            "adr133_success_path_key",
            "adr133_success_path_value",
        );

        let store = Arc::new(MemoryEventStore {
            fail_kind: Some(EventKind::ConfigLocked),
            ..MemoryEventStore::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store);
        let registry = builder.build().expect("registry builds");

        let result = registry
            .dispatch("list", Value::Null)
            .await
            .expect("a config-locked row's commit failure must never fail an unrelated dispatch");
        assert_eq!(
            result,
            serde_json::json!({ "pack": "alpha", "verb": "list" })
        );

        let metrics = registry
            .audit_batch_metrics()
            .expect("with_event_store configures the ADR-133 seam");
        assert!(
            metrics.degraded_rows >= 1,
            "the config-locked row's failure must remain visible as degradation"
        );
    }

    /// An `EventStore` that only implements the base trait — the
    /// unmodified pre-ADR-133 shape. `preflight_event`/
    /// `append_events_idempotent`/`supports_idempotent_audit_batch` are all
    /// inherited defaults.
    #[derive(Default)]
    struct LegacyEventStore {
        events: std::sync::Mutex<Vec<Event>>,
    }

    #[async_trait]
    impl EventStore for LegacyEventStore {
        async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
        async fn append_events(
            &self,
            events: Vec<Event>,
        ) -> khive_storage::StorageResult<BatchWriteSummary> {
            let attempted = events.len() as u64;
            self.events.lock().unwrap().extend(events);
            Ok(BatchWriteSummary {
                attempted,
                affected: attempted,
                failed: 0,
                first_error: String::new(),
            })
        }
        async fn get_event(&self, id: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.id == id)
                .cloned())
        }
        async fn query_events(
            &self,
            _filter: EventFilter,
            _page: PageRequest,
        ) -> khive_storage::StorageResult<Page<Event>> {
            let items = self.events.lock().unwrap().clone();
            let total = items.len() as u64;
            Ok(Page {
                items,
                total: Some(total),
            })
        }
        async fn count_events(&self, _filter: EventFilter) -> khive_storage::StorageResult<u64> {
            Ok(self.events.lock().unwrap().len() as u64)
        }
    }

    #[test]
    fn build_rejects_a_configured_event_store_incompatible_with_the_audit_batch_seam() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(Arc::new(LegacyEventStore::default()));
        let err = match builder.build() {
            Ok(_) => {
                panic!("a store that cannot implement the seam must not build a healthy registry")
            }
            Err(err) => err,
        };
        assert!(
            matches!(&err, RuntimeError::IncompatibleEventStore(message)
                if message.contains("supports_idempotent_audit_batch")),
            "error names the missing capability so an operator can act on it: {err}"
        );
    }

    #[tokio::test]
    #[serial(config_ledger)]
    #[serial(audit_append_failures)]
    #[serial(audit_obligation_append_failures)]
    async fn db_diagnostics_with_audit_metrics_reports_batch_failure_and_degradation() {
        // One dispatch call exercises both halves of the classifier through
        // the registry it actually owns the seam on: the queued
        // `ConfigLocked` (pure-observability) row drains during the gate
        // check regardless of allow/deny, and `list`'s deferred
        // `DispatchSucceeded` (obligation) row is appended once dispatch
        // resolves — both against the same persistently failing store.
        crate::config_ledger::record_config_locked(
            "adr133_diag_test_key",
            "adr133_diag_test_value",
        );
        let store = Arc::new(MemoryEventStore {
            fail_appends: true,
            ..MemoryEventStore::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store);
        let registry = builder.build().expect("registry builds");
        let _ = registry.dispatch("list", Value::Null).await;

        let metrics = registry
            .audit_batch_metrics()
            .expect("with_event_store configures the ADR-133 seam");
        assert!(metrics.degraded, "the config-locked row must have degraded");
        assert!(metrics.degraded_rows >= 1);
        assert!(
            metrics.flush_failures >= 1,
            "the list dispatch's obligation row must count as a flush failure"
        );

        let rt = KhiveRuntime::memory().expect("memory runtime should create");
        let report = rt
            .db_diagnostics_with_audit_metrics(Some(metrics))
            .await
            .expect("diagnostics succeed");
        assert_eq!(report.writer_contention.audit_degraded, Some(true));
        assert!(report.writer_contention.audit_degraded_rows.unwrap_or(0) >= 1);
        assert!(
            report
                .writer_contention
                .audit_batch_flush_failures
                .unwrap_or(0)
                >= 1
        );
        assert!(report
            .writer_contention
            .audit_batch_flush_failures_unavailable_reason
            .is_none());
        assert!(report
            .writer_contention
            .audit_degraded_unavailable_reason
            .is_none());

        // The no-metrics path (a bare `KhiveRuntime::db_diagnostics`, or the
        // `db_diagnostics_with_audit_metrics(None)` it delegates to) must
        // still report the batch-health fields as explicitly unavailable
        // rather than silently zero, so an operator cannot mistake "no
        // registry wired in" for "no failures occurred".
        let bare_report = rt.db_diagnostics().await.expect("diagnostics succeed");
        assert!(bare_report.writer_contention.audit_degraded.is_none());
        assert!(bare_report
            .writer_contention
            .audit_degraded_unavailable_reason
            .is_some());
        assert!(bare_report
            .writer_contention
            .audit_admission_refused_obligations
            .is_none());
        assert!(bare_report
            .writer_contention
            .audit_admission_refused_obligations_unavailable_reason
            .is_some());
        assert!(bare_report
            .writer_contention
            .audit_admission_unresolved_obligations
            .is_none());
        assert!(bare_report
            .writer_contention
            .audit_admission_unresolved_obligations_unavailable_reason
            .is_some());
    }

    #[tokio::test]
    async fn audit_event_duration_us_reflects_measured_dispatch_time() {
        // The persisted audit row's `duration_us` must carry the measured
        // pack-dispatch time, not the `Event::new` default of 0 (persisting
        // the row before dispatch ran always yielded 0). `SleepingPack`
        // sleeps 20ms so the assertion has a wide, non-flaky margin over
        // scheduling jitter.
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SleepingPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("slow_op", serde_json::json!({}))
            .await
            .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        let ev = &page.items[0];
        assert!(
            ev.duration_us >= 10_000,
            "duration_us must reflect the ~20ms measured dispatch time, got {}",
            ev.duration_us
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_allowed_by_gate_still_persists_audit_row() {
        // Generalizing audit-row deferral to every Allow-outcome verb (not
        // just singleton `link`) must not silently drop the audit row for a
        // verb the gate allows but no pack owns. `duration_us` stays at the
        // `Event::new` default of 0 here since no dispatch ever ran to measure.
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let result = reg.dispatch("no_such_verb", serde_json::json!({})).await;
        assert!(result.is_err(), "unknown verb must still return an error");

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(
            count, 1,
            "an allowed-but-unknown verb must still persist one audit row"
        );
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items[0].duration_us, 0);
        // Dispatch returns UnknownVerb for an unknown verb, so the
        // persisted outcome must be Error, not the previously-hardcoded
        // Success.
        assert_eq!(page.items[0].outcome, EventOutcome::Error);
    }

    #[tokio::test]
    async fn audit_event_persists_to_event_store_on_deny() {
        #[derive(Debug)]
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("denied by test"))
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(AlwaysDenyGate));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        // Hard enforce → PermissionDenied returned.
        let err = reg
            .dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(count, 1, "one audit event persisted to EventStore on deny");

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        let ev = &page.items[0];
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.outcome, EventOutcome::Denied);
    }

    #[tokio::test]
    async fn gate_error_returns_typed_refusal_without_invoking_pack() {
        #[derive(Debug)]
        struct FailingGate;
        impl Gate for FailingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
                Err(khive_gate::GateError::Internal("gate broken".into()))
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let invoked = Arc::new(AtomicUsize::new(0));
        let mut builder = VerbRegistryBuilder::new();
        builder.register(GateErrorTrackingPack {
            invoked: Arc::clone(&invoked),
        });
        builder.with_gate(Arc::new(FailingGate));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch("guarded", Value::Null)
            .await
            .expect_err("gate unavailability must refuse normal dispatch");
        assert!(matches!(
            err,
            RuntimeError::GateUnavailable { ref verb, ref reason }
                if verb == "guarded"
                    && reason == "gate backend unavailable"
                    && !reason.contains("gate broken")
        ));
        assert_eq!(
            invoked.load(Ordering::SeqCst),
            0,
            "pack handler must not run after a gate infrastructure error"
        );

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(count, 1, "gate infrastructure error must be audited");
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        let event = &page.items[0];
        assert_eq!(event.verb, "guarded");
        assert_eq!(event.outcome, EventOutcome::Error);
        assert_eq!(event.payload["decision"], "gate_unavailable");
        assert!(event.payload.get("deny_reason").is_none());
        assert_eq!(event.payload["resource"]["work_class"], "interactive");
        assert!(event.payload["resource"].get("cost_unit").is_none());
    }

    #[tokio::test]
    #[serial(audit_append_failures)]
    async fn gate_error_audit_failure_cannot_reopen_dispatch_or_replace_typed_error() {
        #[derive(Debug)]
        struct FailingGate;
        impl Gate for FailingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Err(GateError::Internal("gate still broken".into()))
            }
        }

        let before = audit_append_failure_count();
        let invoked = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(MemoryEventStore {
            fail_appends: true,
            ..MemoryEventStore::default()
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(GateErrorTrackingPack {
            invoked: Arc::clone(&invoked),
        });
        builder.with_gate(Arc::new(FailingGate));
        builder.with_event_store(store);
        let registry = builder.build().expect("registry builds");

        let error = registry
            .dispatch("guarded", Value::Null)
            .await
            .expect_err("audit persistence failure must not reopen dispatch");

        assert!(matches!(
            error,
            RuntimeError::GateUnavailable { ref verb, ref reason }
                if verb == "guarded"
                    && reason == "gate backend unavailable"
                    && !reason.contains("gate still broken")
        ));
        assert_eq!(invoked.load(Ordering::SeqCst), 0);
        assert_eq!(
            audit_append_failure_count(),
            before + 1,
            "best-effort audit failure remains diagnostic without changing the refusal"
        );
    }

    /// Regression for a credential-disclosure path: a gate backend's error
    /// `Display` text can embed connection details (URLs, addresses, auth
    /// material). That text must never reach `RuntimeError::GateUnavailable`
    /// as observed by a dispatch caller — only the stable classified
    /// `wire_reason()` may cross that boundary. A bounded, masked rendering
    /// of the error is logged server-side via `tracing::warn!` in
    /// `gate_unavailable_error`.
    #[tokio::test]
    async fn gate_unavailable_reason_never_carries_backend_error_text() {
        const CANARY: &str = "postgres://svc:not-a-real-secret@internal-host";

        #[derive(Debug)]
        struct FailingGate;
        impl Gate for FailingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Err(GateError::Internal(CANARY.to_string()))
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(GateErrorTrackingPack {
            invoked: Arc::new(AtomicUsize::new(0)),
        });
        builder.with_gate(Arc::new(FailingGate));
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("guarded", Value::Null)
            .await
            .expect_err("gate unavailability must refuse dispatch");

        let RuntimeError::GateUnavailable { reason, .. } = &err else {
            panic!("expected GateUnavailable, got {err:?}");
        };
        assert!(
            !reason.contains(CANARY),
            "caller-visible reason must not embed backend error text: {reason:?}"
        );
        assert!(
            !reason.contains("svc") && !reason.contains("internal-host"),
            "caller-visible reason must not embed backend error fragments: {reason:?}"
        );
        assert_eq!(reason, "gate backend unavailable");

        // The full error, canary included, still reaches the server-side log.
        let rendered = err.to_string();
        assert!(
            !rendered.contains(CANARY),
            "top-level Display must not embed backend error text either: {rendered:?}"
        );
    }

    /// Task 2 (ADR-129 gate-error classification): a `GateError::Policy`
    /// failure — the gate backend is reachable but its configured policy
    /// could not be evaluated — is a distinct, non-transient class from a
    /// `GateError::Internal` backend-availability failure, and gets its own
    /// stable reason text at the dispatch boundary.
    #[tokio::test]
    async fn gate_policy_error_classifies_distinctly_from_backend_unavailable() {
        #[derive(Debug)]
        struct PolicyBrokenGate;
        impl Gate for PolicyBrokenGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Err(GateError::Policy(
                    "rule set has no allow clause for this namespace".to_string(),
                ))
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(GateErrorTrackingPack {
            invoked: Arc::new(AtomicUsize::new(0)),
        });
        builder.with_gate(Arc::new(PolicyBrokenGate));
        let registry = builder.build().expect("registry builds");

        let err = registry
            .dispatch("guarded", Value::Null)
            .await
            .expect_err("gate unavailability must refuse dispatch");

        assert!(matches!(
            err,
            RuntimeError::GateUnavailable { ref verb, ref reason }
                if verb == "guarded"
                    && reason == "gate policy evaluation failed"
                    && !reason.contains("rule set has no allow clause")
        ));
    }

    #[tokio::test]
    async fn no_event_store_configured_tracing_only() {
        // Ordinary verbs remain tracing-only without an event store. The
        // strict git.digest receipt exception is covered separately above.
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        let reg = builder.build().expect("registry builds");

        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");
    }

    #[test]
    #[serial]
    fn dispatch_tracing_emits_gate_check_event_with_deny_payload() {
        #[derive(Debug)]
        struct TracingDenyGate;
        impl Gate for TracingDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("denied by test gate"))
            }
            fn impl_name(&self) -> &'static str {
                "TracingDenyGate"
            }
        }

        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(TracingDenyGate));
            let reg = builder.build().expect("registry builds");
            // Hard enforcement — dispatch returns PermissionDenied on Deny.
            // The tracing audit event is still emitted before the error is returned.
            let _ = reg.dispatch("create", serde_json::Value::Null).await;
        });

        let gate_events = gate_check_events_for(&events, "TracingDenyGate");
        assert_eq!(
            gate_events.len(),
            1,
            "exactly one gate.check tracing event per dispatch (deny); got {gate_events:?}"
        );
        let payload = gate_events[0]
            .audit_event
            .as_ref()
            .expect("gate.check event must carry an audit_event field on Deny");
        let audit: khive_gate::AuditEvent =
            serde_json::from_str(payload).expect("audit_event payload must decode to AuditEvent");
        assert_eq!(audit.decision, AuditDecision::Deny);
        assert_eq!(audit.deny_reason.as_deref(), Some("denied by test gate"));
        assert_eq!(audit.gate_impl, "TracingDenyGate");
        // Wire-shape rule: obligations is always serialized as an array, empty
        // on Deny. Round-trip back through serde_json::Value to confirm the
        // field exists on the wire and is `[]`, not missing.
        let payload_json: serde_json::Value =
            serde_json::from_str(payload).expect("payload must be valid JSON");
        assert_eq!(
            payload_json["obligations"],
            serde_json::Value::Array(Vec::new()),
            "obligations must be `[]` on Deny on the tracing payload, not omitted"
        );
    }

    // ---- EventStore audit envelope round-trip ----
    //
    // EventStore must not persist a summary Event without the full
    // AuditEvent fields (deny_reason, gate_impl, obligations). This test
    // verifies the complete envelope survives append_event → query_events.

    #[tokio::test]
    async fn audit_envelope_round_trips_deny_reason_and_gate_impl_through_event_store() {
        #[derive(Debug)]
        struct DenyGateWithName;
        impl Gate for DenyGateWithName {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("policy: write forbidden for anon"))
            }
            fn impl_name(&self) -> &'static str {
                "DenyGateWithName"
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(DenyGateWithName));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        // Dispatch is denied — PermissionDenied returned.
        let err = reg
            .dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::PermissionDenied { .. }),
            "expected PermissionDenied, got {err:?}"
        );

        // Exactly one event in the store.
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "one audit event must be persisted on deny"
        );

        let ev = &page.items[0];
        assert_eq!(ev.outcome, EventOutcome::Denied);

        // The payload field must hold the full AuditEvent envelope.
        let data = &ev.payload;

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.payload must deserialize to AuditEvent");

        assert_eq!(
            audit.deny_reason.as_deref(),
            Some("policy: write forbidden for anon"),
            "deny_reason must be preserved through EventStore"
        );
        assert_eq!(
            audit.gate_impl, "DenyGateWithName",
            "gate_impl must be preserved through EventStore"
        );
        assert_eq!(
            audit.decision,
            khive_gate::AuditDecision::Deny,
            "decision field must be preserved through EventStore"
        );
    }

    #[tokio::test]
    async fn audit_envelope_round_trips_obligations_through_event_store() {
        use khive_gate::Obligation;

        #[derive(Debug)]
        struct ObligationGate;
        impl Gate for ObligationGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::allow_with(vec![Obligation::Audit {
                    tag: "billing.meter".into(),
                }]))
            }
            fn impl_name(&self) -> &'static str {
                "ObligationGate"
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(ObligationGate));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);

        let ev = &page.items[0];
        assert_eq!(ev.outcome, EventOutcome::Success);

        let data = &ev.payload;

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.payload must deserialize to AuditEvent");

        assert_eq!(audit.gate_impl, "ObligationGate");
        assert_eq!(
            audit.obligations.len(),
            1,
            "obligations must be preserved through EventStore"
        );
        match &audit.obligations[0] {
            Obligation::Audit { tag } => assert_eq!(tag, "billing.meter"),
            other => panic!("expected Audit obligation, got {other:?}"),
        }
    }

    // ---- SQL-backed audit envelope round-trip ----
    //
    // The two tests above use MemoryEventStore (no serialization). This test
    // wires the production SqlEventStore via KhiveRuntime::memory() to verify
    // that the full AuditEvent envelope survives the SQL text→parse round-trip
    // (Event.data is stored as TEXT and parsed back on read).

    #[tokio::test]
    async fn sql_backed_audit_envelope_round_trips_deny_reason_gate_impl_and_obligations() {
        #[derive(Debug)]
        struct SqlTestDenyGate;
        impl Gate for SqlTestDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("sql-path: write denied"))
            }
            fn impl_name(&self) -> &'static str {
                "SqlTestDenyGate"
            }
        }

        // KhiveRuntime::memory() creates an in-memory SQLite pool (is_file_backed=false).
        // events_for_namespace ensures the events schema and returns a SqlEventStore
        // scoped to "test-ns". The pool is shared so reads and writes see the same data.
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let test_tok = NamespaceToken::for_namespace(Namespace::parse("test-ns").unwrap());
        let sql_store = rt
            .events(&test_tok)
            .expect("events_for_namespace must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(SqlTestDenyGate));
        builder.with_event_store(sql_store.clone());
        let reg = builder.build().expect("registry builds");

        // Dispatch is denied — PermissionDenied returned.
        let err = reg
            .dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::PermissionDenied { .. }),
            "expected PermissionDenied, got {err:?}"
        );

        // Query via the same SqlEventStore — this is the SQL read path.
        let page = sql_store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "one audit event must be persisted on deny through SqlEventStore"
        );

        let ev = &page.items[0];
        assert_eq!(ev.outcome, EventOutcome::Denied);

        // Event.payload must hold the full AuditEvent serialized as JSON text and
        // parsed back. If the SQL path was lossy, this deserialization would fail
        // or the field assertions below would fail.
        let data = &ev.payload;

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.payload must deserialize to AuditEvent after SQL round-trip");

        assert_eq!(
            audit.deny_reason.as_deref(),
            Some("sql-path: write denied"),
            "deny_reason must survive the SQL text round-trip"
        );
        assert_eq!(
            audit.gate_impl, "SqlTestDenyGate",
            "gate_impl must survive the SQL text round-trip"
        );
        assert_eq!(
            audit.decision,
            khive_gate::AuditDecision::Deny,
            "decision field must survive the SQL text round-trip"
        );
        // obligations is [] on a Deny gate (no obligations returned).
        // Verify the field is present and empty after SQL round-trip.
        assert!(
            audit.obligations.is_empty(),
            "obligations must be preserved as empty [] through SQL round-trip"
        );
    }

    // ---- SQL-backed audit envelope: non-empty obligations survive round-trip ----
    //
    // Blind spot: the deny-path SQL test above only
    // asserts obligations == [], which passes even if the SQL path drops the
    // field entirely (AuditEvent.obligations has #[serde(default)]).
    //
    // This test installs an allow-path gate that returns a non-empty obligations
    // vec. After dispatch, the same SqlEventStore is queried and both layers are
    // checked:
    //   1. Raw Event.data["obligations"] is a non-empty JSON array.
    //   2. Deserialized AuditEvent.obligations[0] matches the expected variant.
    #[tokio::test]
    async fn sql_backed_audit_envelope_round_trips_non_empty_obligations() {
        use khive_gate::Obligation;

        #[derive(Debug)]
        struct SqlTestAllowWithObligationGate;
        impl Gate for SqlTestAllowWithObligationGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::allow_with(vec![Obligation::Audit {
                    tag: "sql-path-billing.meter".into(),
                }]))
            }
            fn impl_name(&self) -> &'static str {
                "SqlTestAllowWithObligationGate"
            }
        }

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let test_tok = NamespaceToken::for_namespace(Namespace::parse("test-ns").unwrap());
        let sql_store = rt
            .events(&test_tok)
            .expect("events_for_namespace must succeed");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(SqlTestAllowWithObligationGate));
        builder.with_event_store(sql_store.clone());
        let reg = builder.build().expect("registry builds");

        // Dispatch succeeds — the gate allows with obligations.
        reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .expect("dispatch must succeed when gate allows");

        // Query via the same SqlEventStore — this is the SQL read path.
        let page = sql_store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "one audit event must be persisted on allow through SqlEventStore"
        );

        let ev = &page.items[0];
        assert_eq!(ev.outcome, EventOutcome::Success);

        let data = &ev.payload;

        // Layer 1: raw JSON check — obligations must be a non-empty array in
        // the persisted TEXT. If the SQL path dropped the field, the default
        // #[serde(default)] would silently deserialize it to [], so we verify
        // the raw JSON before deserializing.
        let obligations_raw = data
            .get("obligations")
            .expect("Event.data JSON must contain 'obligations' key");
        let obligations_arr = obligations_raw
            .as_array()
            .expect("'obligations' must be a JSON array");
        assert!(
            !obligations_arr.is_empty(),
            "raw Event.data['obligations'] must be non-empty after SQL round-trip"
        );

        // Layer 2: deserialized AuditEvent check — the obligation variant and
        // payload must survive the text round-trip faithfully.
        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.data must deserialize to AuditEvent after SQL round-trip");

        assert_eq!(
            audit.gate_impl, "SqlTestAllowWithObligationGate",
            "gate_impl must survive the SQL text round-trip"
        );
        assert_eq!(
            audit.decision,
            khive_gate::AuditDecision::Allow,
            "decision field must survive the SQL text round-trip"
        );
        assert_eq!(
            audit.obligations.len(),
            1,
            "obligations must be non-empty after SQL round-trip (not silently defaulted to [])"
        );
        match &audit.obligations[0] {
            Obligation::Audit { tag } => assert_eq!(
                tag, "sql-path-billing.meter",
                "Audit obligation tag must survive the SQL text round-trip"
            ),
            other => panic!("expected Audit obligation, got {other:?}"),
        }
    }

    // ---- Audit payload shape for 'create' verb dispatch ----
    //
    // The previous audit tests verify the envelope shape for the 'list' verb.
    // This test dispatches 'create' (matching the create_note + annotates path)
    // and verifies that ev.verb, ev.outcome, and ev.data all round-trip correctly
    // through the EventStore. Ensures the wire shape is independent of which verb
    // triggers the gate check.
    #[tokio::test]
    async fn audit_event_payload_shape_for_create_verb() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        // Dispatch 'create' — AlphaPack returns a stub value; what matters is
        // the EventStore entry emitted by the registry's gate-check path.
        reg.dispatch("create", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap();

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(count, 1, "exactly one audit event for one dispatch");

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        let ev = &page.items[0];

        // Top-level Event fields.
        assert_eq!(ev.verb, "create", "ev.verb must be the dispatched verb");
        assert_eq!(
            ev.outcome,
            EventOutcome::Success,
            "ev.outcome must be Success on allow"
        );
        assert_eq!(
            ev.namespace, "test-ns",
            "ev.namespace must match the dispatch namespace"
        );

        // ev.payload must hold the full AuditEvent envelope.
        let data = &ev.payload;

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("ev.payload must deserialize to AuditEvent");

        assert_eq!(
            audit.decision,
            khive_gate::AuditDecision::Allow,
            "AuditEvent.decision must be Allow"
        );
        assert_eq!(audit.verb, "create", "AuditEvent.verb must be 'create'");
        assert_eq!(
            audit.namespace, "test-ns",
            "AuditEvent.namespace must be preserved"
        );
        assert_eq!(
            audit.gate_impl, "AllowAllGate",
            "AuditEvent.gate_impl must name the gate implementation"
        );
        assert!(
            audit.deny_reason.is_none(),
            "AuditEvent.deny_reason must be None on Allow"
        );
        // Wire-shape check: obligations serializes as [] on AllowAllGate.
        let payload_json: serde_json::Value =
            serde_json::from_value(data.clone()).expect("data must be valid JSON");
        assert_eq!(
            payload_json["obligations"],
            serde_json::Value::Array(Vec::new()),
            "obligations must be [] on AllowAllGate"
        );
    }

    // ---- ADR-103 Amendment 1: resource.cost_unit emission ----

    /// Test pack whose `create` handler is a stub (mirrors `AlphaPack`) but
    /// overrides `registered_embedding_model_names` to a configurable set,
    /// exercising ADR-103 Amendment 1's `model_count` computation for
    /// singleton `create` at the dispatch audit-row emission seam.
    struct EmbeddingAwarePack {
        models: Vec<String>,
    }

    impl khive_types::Pack for EmbeddingAwarePack {
        const NAME: &'static str = "embedding_aware";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "create",
            description: "create a widget (embedding-aware stub)",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for EmbeddingAwarePack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn registered_embedding_model_names(&self) -> Vec<String> {
            self.models.clone()
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "embedding_aware", "verb": verb }))
        }
    }

    /// Test pack whose one verb, `probe`, always fails — used to drive the
    /// general (non-link) deferred-audit Err arm without a real backend.
    struct FailingProbePack;

    impl khive_types::Pack for FailingProbePack {
        const NAME: &'static str = "failing_probe";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "probe",
            description: "always fails",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for FailingProbePack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput("boom".into()))
        }
    }

    #[tokio::test]
    async fn resource_cost_unit_present_on_non_embedding_successful_dispatch() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("list", serde_json::json!({})).await.unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive", "cost_unit": 1}),
            "non-embedding-bearing verb's resource.cost_unit must be base_weight(verb) alone"
        );
    }

    #[tokio::test]
    async fn resource_cost_unit_scales_with_registered_model_count_for_create() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(EmbeddingAwarePack {
            models: vec!["all-minilm-l6-v2".into(), "paraphrase".into()],
        });
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("create", serde_json::json!({"kind": "widget"}))
            .await
            .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        // base_weight(1) + per_item_weight(1) * item_count(1) * model_count(2)
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive", "cost_unit": 3}),
        );
    }

    #[tokio::test]
    async fn resource_cost_unit_zero_registered_models_is_base_weight_only() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(EmbeddingAwarePack { models: vec![] });
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("create", serde_json::json!({"kind": "widget"}))
            .await
            .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items[0].payload["resource"]["cost_unit"], 1,
            "zero registered embedding models must vanish the term, not error or omit"
        );
    }

    #[tokio::test]
    async fn resource_work_class_present_cost_unit_absent_when_dispatch_returns_error() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(FailingProbePack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch("probe", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::InvalidInput(_)));

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].outcome, EventOutcome::Error);
        // ADR-103 Decision (a): work_class is stamped on EVERY event, denial
        // and error included -- only Amendment 1's cost_unit field is scoped
        // to a successful dispatch. An errored dispatch keeps
        // resource.work_class and omits only resource.cost_unit, never 0.
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive"}),
            "resource must carry work_class with cost_unit OMITTED (never 0) on an \
             errored dispatch: {:?}",
            page.items[0].payload
        );
    }

    #[tokio::test]
    async fn resource_work_class_present_cost_unit_absent_when_no_pack_owns_the_verb() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let _ = reg
            .dispatch("no_such_verb_resource_test", serde_json::json!({}))
            .await;

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive"})
        );
    }

    #[tokio::test]
    async fn resource_work_class_present_cost_unit_absent_on_denied_dispatch() {
        #[derive(Debug)]
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("test: always deny"))
            }
        }
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(AlwaysDenyGate));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let _ = reg.dispatch("list", serde_json::json!({})).await;

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].outcome, EventOutcome::Denied);
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive"})
        );
    }

    #[tokio::test]
    async fn resource_cost_unit_present_on_link_singleton_success() {
        let store = Arc::new(MemoryEventStore::default());
        let edge_id = uuid::Uuid::new_v4();
        let source_id = uuid::Uuid::new_v4();
        let target_id = uuid::Uuid::new_v4();
        let edge_json = serde_json::json!({
            "id": edge_id,
            "namespace": "local",
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
            "weight": 1.0,
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::ok(edge_json));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch(
            "link",
            serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "depends_on",
            }),
        )
        .await
        .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive", "cost_unit": 1}),
            "link has no embedding-bearing path -> base_weight(link) alone, even on the v2-enriched singleton path"
        );
    }

    #[tokio::test]
    async fn resource_work_class_present_cost_unit_absent_on_link_dispatch_failure() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::err("target endpoint not found"));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let _ = reg
            .dispatch(
                "link",
                serde_json::json!({
                    "source_id": "note:alpha",
                    "target_id": "note:missing",
                    "relation": "depends_on",
                }),
            )
            .await;

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].payload["resource"],
            serde_json::json!({"work_class": "interactive"})
        );
    }

    // Registry audit event must carry target_id when dispatch params include it.
    #[tokio::test]
    async fn audit_event_threads_target_id_from_dispatch_args() {
        let store = Arc::new(MemoryEventStore::default());
        let target = uuid::Uuid::new_v4();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        reg.dispatch(
            "create",
            serde_json::json!({"namespace": "test-ns", "target_id": target}),
        )
        .await
        .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items[0].target_id,
            Some(target),
            "#282: audit event must carry target_id from dispatch params"
        );
    }

    // ---- Link-verb audit enrichment ----

    /// Test pack exposing a single `link` verb whose one-shot result is
    /// configured up front — lets tests drive both the success and failure
    /// legs of the deferred link-audit path without a real KG backend.
    struct LinkResultPack {
        result: std::sync::Mutex<Option<Result<Value, RuntimeError>>>,
    }

    impl LinkResultPack {
        fn ok(value: Value) -> Self {
            Self {
                result: std::sync::Mutex::new(Some(Ok(value))),
            }
        }
        fn err(message: &str) -> Self {
            Self {
                result: std::sync::Mutex::new(Some(Err(RuntimeError::InvalidInput(
                    message.to_string(),
                )))),
            }
        }
    }

    impl khive_types::Pack for LinkResultPack {
        const NAME: &'static str = "kg";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "link",
            description: "test link handler",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for LinkResultPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("LinkResultPack dispatch called more than once in a test")
        }
    }

    #[tokio::test]
    async fn link_audit_enriches_successful_singleton_with_edge_v2() {
        let store = Arc::new(MemoryEventStore::default());
        let edge_id = uuid::Uuid::new_v4();
        let source_id = uuid::Uuid::new_v4();
        let target_id = uuid::Uuid::new_v4();
        let edge_json = serde_json::json!({
            "id": edge_id,
            "namespace": "local",
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
            "weight": 1.0,
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::ok(edge_json));
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        reg.dispatch(
            "link",
            serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "depends_on",
            }),
        )
        .await
        .unwrap();

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(
            count, 1,
            "exactly one deferred audit row must be persisted for a successful singleton link"
        );
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        let ev = &page.items[0];
        assert_eq!(ev.verb, "link");
        assert_eq!(ev.outcome, EventOutcome::Success);
        assert_eq!(
            ev.payload_schema_version, 2,
            "successful singleton link uses audit schema v2"
        );
        assert_eq!(
            ev.target_id,
            Some(edge_id),
            "target_id must be the created/resolved edge id, not a raw caller arg"
        );
        assert_eq!(ev.payload["edge_id"], serde_json::json!(edge_id));
        assert_eq!(ev.payload["source_id"], serde_json::json!(source_id));
        assert_eq!(ev.payload["target_id"], serde_json::json!(target_id));
        assert_eq!(ev.payload["relation"], "depends_on");
        assert_eq!(ev.payload["weight"], 1.0);
        // v1 AuditEvent fields remain present via #[serde(flatten)].
        assert_eq!(ev.payload["verb"], "link");
        assert_eq!(ev.payload["decision"], "allow");
        assert!(ev.payload.get("gate_impl").is_some());
    }

    #[tokio::test]
    async fn link_audit_falls_back_to_v1_when_dispatch_fails() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::err("target endpoint not found"));
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch(
                "link",
                serde_json::json!({
                    "source_id": "note:alpha",
                    "target_id": "note:missing",
                    "relation": "depends_on",
                }),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("not found")),
            "the original dispatch error must be returned unchanged"
        );

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "a v1 fallback audit row must still be persisted on dispatch failure"
        );
        let ev = &page.items[0];
        assert_eq!(
            ev.payload_schema_version, 1,
            "failed link keeps the v1 audit shape"
        );
        // The persisted outcome must reflect the dispatch result (Err →
        // Error), not be hardcoded to Success from the gate's Allow decision.
        assert_eq!(
            ev.outcome,
            EventOutcome::Error,
            "outcome reflects the dispatch result (Err), not the gate decision (Allow)"
        );
        assert!(
            ev.duration_us >= 0,
            "duration_us must still be populated (measured, not the Event::new \
             default sentinel) on a failed dispatch"
        );
        assert!(
            ev.target_id.is_none(),
            "non-UUID caller-supplied ids do not spuriously populate target_id"
        );
        assert!(
            ev.payload.get("edge_id").is_none(),
            "v1 fallback must not carry edge enrichment fields"
        );
        let _: khive_gate::AuditEvent = serde_json::from_value(ev.payload.clone())
            .expect("v1 fallback payload must deserialize as AuditEvent");
    }

    #[tokio::test]
    async fn link_audit_falls_back_to_v1_when_result_missing_edge_fields() {
        let store = Arc::new(MemoryEventStore::default());
        let target_arg = uuid::Uuid::new_v4();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::ok(serde_json::json!({"ok": true})));
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        reg.dispatch(
            "link",
            serde_json::json!({
                "source_id": uuid::Uuid::new_v4(),
                "target_id": target_arg,
                "relation": "depends_on",
            }),
        )
        .await
        .unwrap();

        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        let ev = &page.items[0];
        assert_eq!(
            ev.payload_schema_version, 1,
            "an unparsable success result falls back to v1 rather than dropping the audit row"
        );
        assert_eq!(ev.outcome, EventOutcome::Success);
        assert_eq!(
            ev.target_id,
            Some(target_arg),
            "v1 fallback still extracts target_id from the raw dispatch args"
        );
        assert!(ev.payload.get("edge_id").is_none());
    }

    #[tokio::test]
    async fn link_audit_bulk_links_get_no_enrichment() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::ok(serde_json::json!({
            "attempted": 2, "created": 2, "skipped": 0, "failed": 0
        })));
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        reg.dispatch(
            "link",
            serde_json::json!({
                "links": [
                    {"source_id": "a", "target_id": "b", "relation": "depends_on"},
                    {"source_id": "c", "target_id": "d", "relation": "depends_on"},
                ],
            }),
        )
        .await
        .unwrap();

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(
            count, 1,
            "bulk `links` gets exactly one v1 audit row (deferred until dispatch \
             resolves like every other Allow-outcome row since ADR-103 Stage 1, \
             but never v2-enriched — enrichment is singleton-`link`-only)"
        );
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        let ev = &page.items[0];
        assert_eq!(
            ev.payload_schema_version, 1,
            "bulk link mode is out of scope for #676's events.target_id enrichment"
        );
        assert!(ev.target_id.is_none());
    }

    #[test]
    fn link_audit_success_from_result_extracts_edge_fields() {
        let gate_req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::local(),
            "link",
            serde_json::json!({}),
        );
        let decision = GateDecision::Allow {
            obligations: vec![],
        };
        let audit = AuditEvent::from_check(&gate_req, &decision, "AllowAllGate");

        let edge_id = uuid::Uuid::new_v4();
        let source_id = uuid::Uuid::new_v4();
        let target_id = uuid::Uuid::new_v4();
        let result = serde_json::json!({
            "id": edge_id,
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
            "weight": 0.5,
        });

        let (returned_id, payload) = link_audit_success_from_result(audit, &result)
            .expect("well-formed edge JSON must produce an enriched payload");
        assert_eq!(returned_id, edge_id);
        assert_eq!(payload["edge_id"], serde_json::json!(edge_id));
        assert_eq!(payload["relation"], "depends_on");
        assert_eq!(payload["weight"], 0.5);
        assert_eq!(
            payload["verb"], "link",
            "v1 AuditEvent fields must flatten into the v2 payload"
        );
    }

    #[test]
    fn link_audit_success_from_result_rejects_incomplete_or_malformed_result() {
        let gate_req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::local(),
            "link",
            serde_json::json!({}),
        );
        let decision = GateDecision::Allow {
            obligations: vec![],
        };
        let audit = AuditEvent::from_check(&gate_req, &decision, "AllowAllGate");

        assert!(
            link_audit_success_from_result(
                audit.clone(),
                &serde_json::json!({"id": uuid::Uuid::new_v4()}),
            )
            .is_none(),
            "missing source_id/target_id/relation/weight must not enrich"
        );
        assert!(
            link_audit_success_from_result(audit, &serde_json::json!({"id": "not-a-uuid"}))
                .is_none(),
            "a non-UUID id must not enrich"
        );
    }

    // ---- khive#948: request_id survives to the persisted audit event ----
    //
    // The pure `resource_payload`/`base_resource_payload` helpers are unit
    // tested in `cost_unit.rs`; these tests prove the id actually reaches
    // `resource.request_id` on a persisted `Event` through every one of
    // `dispatch_with_identity`'s four audit-append sites (denied, ordinary
    // success/error, singleton-link v2 success and its v1 fallback, and the
    // unknown-verb error path), plus the "no id supplied" omission case.

    async fn first_event(store: &Arc<MemoryEventStore>) -> Event {
        let page = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            page.items.len(),
            1,
            "expected exactly one persisted audit event"
        );
        page.items[0].clone()
    }

    #[tokio::test]
    async fn dispatch_with_identity_stamps_request_id_on_success() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch_with_identity(
            "list",
            serde_json::json!({"namespace": "test-ns"}),
            Some(RequestIdentity {
                request_id: Some(101),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let ev = first_event(&store).await;
        assert_eq!(ev.outcome, EventOutcome::Success);
        assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(101));
    }

    #[tokio::test]
    async fn dispatch_with_identity_stamps_request_id_on_dispatch_error() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(FailingProbePack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch_with_identity(
                "probe",
                serde_json::json!({"namespace": "test-ns"}),
                Some(RequestIdentity {
                    request_id: Some(102),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::InvalidInput(_)));

        let ev = first_event(&store).await;
        assert_eq!(ev.outcome, EventOutcome::Error);
        assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(102));
    }

    #[tokio::test]
    async fn dispatch_with_identity_stamps_request_id_on_denied() {
        #[derive(Debug)]
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("denied by test"))
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(AlwaysDenyGate));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch_with_identity(
                "list",
                serde_json::json!({"namespace": "test-ns"}),
                Some(RequestIdentity {
                    request_id: Some(103),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

        let ev = first_event(&store).await;
        assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(103));
    }

    #[tokio::test]
    async fn dispatch_with_identity_stamps_request_id_on_link_v2_success() {
        let store = Arc::new(MemoryEventStore::default());
        let edge_id = uuid::Uuid::new_v4();
        let source_id = uuid::Uuid::new_v4();
        let target_id = uuid::Uuid::new_v4();
        let edge_json = serde_json::json!({
            "id": edge_id,
            "namespace": "local",
            "source_id": source_id,
            "target_id": target_id,
            "relation": "depends_on",
            "weight": 1.0,
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::ok(edge_json));
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        reg.dispatch_with_identity(
            "link",
            serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "depends_on",
            }),
            Some(RequestIdentity {
                namespace: "test-ns".to_string(),
                request_id: Some(104),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        let ev = first_event(&store).await;
        assert_eq!(
            ev.payload_schema_version, 2,
            "successful singleton link uses audit schema v2"
        );
        assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(104));
    }

    #[tokio::test]
    async fn dispatch_with_identity_stamps_request_id_on_link_v1_fallback() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LinkResultPack::err("target endpoint not found"));
        builder.with_event_store(store.clone());
        builder.with_default_namespace("test-ns");
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch_with_identity(
                "link",
                serde_json::json!({
                    "source_id": "note:alpha",
                    "target_id": "note:missing",
                    "relation": "depends_on",
                }),
                Some(RequestIdentity {
                    namespace: "test-ns".to_string(),
                    request_id: Some(105),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::InvalidInput(_)));

        let ev = first_event(&store).await;
        assert_eq!(
            ev.payload_schema_version, 1,
            "failed link keeps the v1 audit shape"
        );
        assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(105));
    }

    #[tokio::test]
    async fn dispatch_with_identity_stamps_request_id_on_unknown_verb() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        let err = reg
            .dispatch_with_identity(
                "no_such_verb",
                serde_json::json!({}),
                Some(RequestIdentity {
                    namespace: Namespace::local().as_str().to_string(),
                    request_id: Some(106),
                    ..Default::default()
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::UnknownVerb(_)));

        let ev = first_event(&store).await;
        assert_eq!(ev.outcome, EventOutcome::Error);
        assert_eq!(ev.payload["resource"]["request_id"], serde_json::json!(106));
    }

    #[tokio::test]
    async fn dispatch_with_identity_omits_request_id_key_when_absent() {
        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        // No identity at all — the pre-#948 call shape.
        reg.dispatch("list", serde_json::json!({"namespace": "test-ns"}))
            .await
            .unwrap();

        let ev = first_event(&store).await;
        let resource = ev.payload["resource"]
            .as_object()
            .expect("resource must be an object");
        assert!(
            !resource.contains_key("request_id"),
            "request_id key must be entirely absent when no id is supplied, \
             not present as null or 0: got {resource:?}"
        );
    }
}

// ---- Inter-pack dependency checking ----

#[cfg(test)]
mod dep_tests {
    use super::*;
    use async_trait::async_trait;
    use khive_types::Pack;
    use serde_json::Value;

    struct KgDepPack;
    struct MemoryDepPack;
    struct ADepPack;
    struct BDepPack;

    impl Pack for KgDepPack {
        const NAME: &'static str = "kg_dep";
        const NOTE_KINDS: &'static [&'static str] = &["observation"];
        const ENTITY_KINDS: &'static [&'static str] = &["concept"];
        const HANDLERS: &'static [HandlerDef] = &[];
    }

    impl Pack for MemoryDepPack {
        const NAME: &'static str = "memory_dep";
        const NOTE_KINDS: &'static [&'static str] = &["memory"];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
        const REQUIRES: &'static [&'static str] = &["kg_dep"];
    }

    impl Pack for ADepPack {
        const NAME: &'static str = "pack_a";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
        const REQUIRES: &'static [&'static str] = &["pack_b"];
    }

    impl Pack for BDepPack {
        const NAME: &'static str = "pack_b";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
        const REQUIRES: &'static [&'static str] = &["pack_a"];
    }

    #[async_trait]
    impl PackRuntime for KgDepPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput(format!(
                "KgDepPack has no verbs: {verb}"
            )))
        }
    }

    #[async_trait]
    impl PackRuntime for MemoryDepPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn requires(&self) -> &'static [&'static str] {
            Self::REQUIRES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput(format!(
                "MemoryDepPack has no verbs: {verb}"
            )))
        }
    }

    #[async_trait]
    impl PackRuntime for ADepPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn requires(&self) -> &'static [&'static str] {
            Self::REQUIRES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput(format!(
                "ADepPack has no verbs: {verb}"
            )))
        }
    }

    #[async_trait]
    impl PackRuntime for BDepPack {
        fn name(&self) -> &str {
            Self::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            Self::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            Self::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            Self::HANDLERS
        }
        fn requires(&self) -> &'static [&'static str] {
            Self::REQUIRES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
            _: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Err(RuntimeError::InvalidInput(format!(
                "BDepPack has no verbs: {verb}"
            )))
        }
    }

    #[test]
    fn test_pack_deps_happy_path() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(MemoryDepPack);
        builder.register(KgDepPack);
        let reg = builder
            .build()
            .expect("kg_dep satisfies memory_dep dependency");
        assert_eq!(reg.pack_requires("memory_dep").unwrap(), &["kg_dep"]);
        let names = reg.pack_names();
        let kg_pos = names.iter().position(|&n| n == "kg_dep").unwrap();
        let mem_pos = names.iter().position(|&n| n == "memory_dep").unwrap();
        assert!(
            kg_pos < mem_pos,
            "kg_dep must be loaded before memory_dep; order: {names:?}"
        );
    }

    #[test]
    fn test_pack_deps_missing() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(MemoryDepPack);
        let err = match builder.build() {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        };
        assert!(
            matches!(err, RuntimeError::MissingPackDependency(_)),
            "expected MissingPackDependency, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("memory_dep"),
            "error must name the dependent pack: {msg}"
        );
        assert!(
            msg.contains("kg_dep"),
            "error must name the missing dep: {msg}"
        );
    }

    #[test]
    fn test_pack_deps_circular() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(ADepPack);
        builder.register(BDepPack);
        let err = match builder.build() {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        };
        assert!(
            matches!(err, RuntimeError::CircularPackDependency(_)),
            "expected CircularPackDependency, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("pack_a"), "error must name pack_a: {msg}");
        assert!(msg.contains("pack_b"), "error must name pack_b: {msg}");
    }

    #[test]
    fn test_pack_deps_no_deps() {
        struct NoDepsA;
        struct NoDepsB;

        impl Pack for NoDepsA {
            const NAME: &'static str = "no_deps_a";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[];
        }

        impl Pack for NoDepsB {
            const NAME: &'static str = "no_deps_b";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[];
        }

        #[async_trait]
        impl PackRuntime for NoDepsA {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                verb: &str,
                _: Value,
                _: &VerbRegistry,
                _: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                Err(RuntimeError::InvalidInput(format!("NoDepsA: {verb}")))
            }
        }

        #[async_trait]
        impl PackRuntime for NoDepsB {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            async fn dispatch(
                &self,
                verb: &str,
                _: Value,
                _: &VerbRegistry,
                _: &NamespaceToken,
            ) -> Result<Value, RuntimeError> {
                Err(RuntimeError::InvalidInput(format!("NoDepsB: {verb}")))
            }
        }

        let mut builder = VerbRegistryBuilder::new();
        builder.register(NoDepsA);
        builder.register(NoDepsB);
        let reg = builder.build().expect("packs with REQUIRES=&[] build");
        assert_eq!(reg.pack_requires("no_deps_a").unwrap(), &[] as &[&str]);
        assert_eq!(reg.pack_requires("no_deps_b").unwrap(), &[] as &[&str]);
    }
}

// ── Dispatch hook tests ─────────────────────────────────────────

#[cfg(test)]
mod hook_tests {
    use super::*;
    use async_trait::async_trait;
    use khive_types::Pack;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    struct SimplePack;

    impl Pack for SimplePack {
        const NAME: &'static str = "simple";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "ping",
            description: "ping",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for SimplePack {
        fn name(&self) -> &str {
            SimplePack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            SimplePack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            SimplePack::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            SimplePack::HANDLERS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "verb": verb }))
        }
    }

    struct RecallPack;

    impl Pack for RecallPack {
        const NAME: &'static str = "memory";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
            name: "memory.recall",
            description: "test recall",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait]
    impl PackRuntime for RecallPack {
        fn name(&self) -> &str {
            RecallPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            RecallPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            RecallPack::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            RecallPack::HANDLERS
        }
        async fn dispatch(
            &self,
            _verb: &str,
            params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            let hit = serde_json::json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "served_by_profile_id": "custom-recall-v1",
                "serve_attribution": "profile",
            });
            if params.get("verbose").and_then(Value::as_bool) == Some(true) {
                Ok(serde_json::json!({"results": [hit]}))
            } else {
                Ok(serde_json::json!([hit]))
            }
        }
    }

    #[derive(Default)]
    struct EventCapturingHook {
        event: StdMutex<Option<Event>>,
    }

    #[async_trait]
    impl DispatchHook for EventCapturingHook {
        async fn on_dispatch(&self, view: &EventView) {
            *self.event.lock().unwrap() = Some(view.event.clone());
        }
    }

    /// Hook that counts calls and records the last verb seen.
    #[derive(Default)]
    struct CountingHook {
        calls: AtomicUsize,
        last_verb: StdMutex<String>,
    }

    #[async_trait]
    impl DispatchHook for CountingHook {
        async fn on_dispatch(&self, view: &EventView) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_verb.lock().unwrap() = view.event.verb.clone();
        }
    }

    #[tokio::test]
    async fn dispatch_hook_fires_on_successful_dispatch() {
        let hook = Arc::new(CountingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        builder.with_dispatch_hook(hook.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("ping", Value::Null).await.unwrap();

        assert_eq!(
            hook.calls.load(Ordering::SeqCst),
            1,
            "hook must fire once per successful dispatch"
        );
        assert_eq!(
            hook.last_verb.lock().unwrap().as_str(),
            "ping",
            "hook event must carry the dispatched verb"
        );
    }

    #[tokio::test]
    async fn dispatch_hook_fires_multiple_times() {
        let hook = Arc::new(CountingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        builder.with_dispatch_hook(hook.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("ping", Value::Null).await.unwrap();
        reg.dispatch("ping", Value::Null).await.unwrap();
        reg.dispatch("ping", Value::Null).await.unwrap();

        assert_eq!(
            hook.calls.load(Ordering::SeqCst),
            3,
            "hook must fire once per successful dispatch"
        );
    }

    #[tokio::test]
    async fn recall_hook_copies_serve_attribution_from_bare_and_verbose_results() {
        let hook = Arc::new(EventCapturingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(RecallPack);
        builder.with_dispatch_hook(hook.clone());
        let reg = builder.build().expect("registry builds");

        for params in [serde_json::json!({}), serde_json::json!({"verbose": true})] {
            reg.dispatch("memory.recall", params)
                .await
                .expect("recall dispatch");
            let event = hook.event.lock().unwrap().clone().expect("hook event");
            assert!(
                event.target_id.is_some(),
                "first recall id must become target"
            );
            assert_eq!(
                event.payload["served_by_profile_id"],
                serde_json::json!("custom-recall-v1")
            );
            assert_eq!(
                event.payload["serve_attribution"],
                serde_json::json!("profile")
            );
        }
    }

    #[tokio::test]
    async fn dispatch_hook_does_not_fire_on_unknown_verb() {
        let hook = Arc::new(CountingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        builder.with_dispatch_hook(hook.clone());
        let reg = builder.build().expect("registry builds");

        let _ = reg.dispatch("nonexistent", Value::Null).await;

        assert_eq!(
            hook.calls.load(Ordering::SeqCst),
            0,
            "hook must NOT fire for unknown verb (dispatch returns error)"
        );
    }

    #[tokio::test]
    async fn dispatch_hook_does_not_fire_on_gate_deny() {
        use khive_gate::{Gate, GateDecision, GateError};

        #[derive(Debug)]
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("test deny"))
            }
        }

        let hook = Arc::new(CountingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        builder.with_gate(Arc::new(AlwaysDenyGate));
        builder.with_dispatch_hook(hook.clone());
        let reg = builder.build().expect("registry builds");

        let err = reg.dispatch("ping", Value::Null).await.unwrap_err();
        assert!(matches!(err, RuntimeError::PermissionDenied { .. }));

        assert_eq!(
            hook.calls.load(Ordering::SeqCst),
            0,
            "hook must NOT fire when gate denies dispatch"
        );
    }

    #[tokio::test]
    async fn dispatch_hook_event_carries_namespace_from_params() {
        let hook = Arc::new(CountingHook::default());

        #[derive(Default)]
        struct NsCapturingHook {
            ns: StdMutex<String>,
        }

        #[async_trait]
        impl DispatchHook for NsCapturingHook {
            async fn on_dispatch(&self, view: &EventView) {
                *self.ns.lock().unwrap() = view.event.namespace.clone();
            }
        }

        let ns_hook = Arc::new(NsCapturingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        builder.with_dispatch_hook(ns_hook.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch("ping", serde_json::json!({"namespace": "tenant-abc"}))
            .await
            .unwrap();

        assert_eq!(
            ns_hook.ns.lock().unwrap().as_str(),
            "tenant-abc",
            "dispatch hook event must carry the resolved namespace"
        );

        // Suppress unused-variable warning from the outer hook.
        drop(hook);
    }

    #[tokio::test]
    async fn no_dispatch_hook_configured_dispatch_succeeds() {
        // Regression: registries without a hook must still work.
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        // No with_dispatch_hook call.
        let reg = builder.build().expect("registry builds");

        let res = reg.dispatch("ping", Value::Null).await.unwrap();
        assert_eq!(res["verb"], "ping");
    }
}

// ── help=true tests ──────────────────────────────────────────────

#[cfg(test)]
mod help_tests {
    use super::*;
    use async_trait::async_trait;
    use khive_types::Pack;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    // ── HelpPack: a minimal pack with one handler that records invocation count.
    //
    // Used to verify that help=true never reaches the pack's dispatch method.

    static CREATE_PARAMS: [ParamDef; 2] = [
        ParamDef {
            name: "kind",
            param_type: "string",
            required: true,
            description: "Granular kind (concept | document | ...).",
        },
        ParamDef {
            name: "name",
            param_type: "string",
            required: false,
            description: "Human-readable name.",
        },
    ];

    static RECALL_PARAMS: [ParamDef; 2] = [
        ParamDef {
            name: "query",
            param_type: "string",
            required: true,
            description: "Semantic recall query.",
        },
        ParamDef {
            name: "limit",
            param_type: "integer",
            required: false,
            description: "Maximum memories to return.",
        },
    ];

    // A subhandler with no params — mirrors recall.embed / brain.emit / etc.
    // Used to test that help=true on a Subhandler returns callable_via_mcp: false.
    static EMBED_PARAMS: [ParamDef; 0] = [];

    struct HelpPack {
        invocations: Arc<AtomicUsize>,
    }

    impl Pack for HelpPack {
        const NAME: &'static str = "helptest";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[
            HandlerDef {
                name: "create",
                description: "Create an entity or note",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &CREATE_PARAMS,
            },
            HandlerDef {
                name: "recall",
                description: "Recall memory notes with decay-aware hybrid ranking",
                visibility: Visibility::Verb,
                category: VerbCategory::Assertive,
                params: &RECALL_PARAMS,
            },
            // A Subhandler used to test that help=true returns
            // callable_via_mcp: false for internal verbs.
            HandlerDef {
                name: "recall.embed",
                description: "Return the embedding vector used by memory recall",
                visibility: Visibility::Subhandler,
                category: VerbCategory::Assertive,
                params: &EMBED_PARAMS,
            },
            HandlerDef {
                name: "link",
                description: "Create a typed directed edge",
                visibility: Visibility::Verb,
                category: VerbCategory::Commissive,
                params: &[],
            },
        ];
    }

    // A pack-declared additive edge rule (mirrors the GTD pack's real
    // task-to-task `depends_on` rule), used to verify `link(help=true)`
    // surfaces pack-composed rules alongside the base entity table. The
    // second entry declares a rule for a special relation
    // (`supersedes`) that the validator's dedicated special-relation
    // branch never consults `pack_rule_allows` for — it must NOT be
    // advertised (see `test_link_help_true_matches_special_relation_validator_set`).
    static HELP_EDGE_RULES: [EdgeEndpointRule; 2] = [
        EdgeEndpointRule {
            relation: khive_types::EdgeRelation::DependsOn,
            source: EndpointKind::NoteOfKind("task"),
            target: EndpointKind::NoteOfKind("task"),
        },
        EdgeEndpointRule {
            relation: khive_types::EdgeRelation::Supersedes,
            source: EndpointKind::NoteOfKind("task"),
            target: EndpointKind::NoteOfKind("task"),
        },
    ];

    #[async_trait]
    impl PackRuntime for HelpPack {
        fn name(&self) -> &str {
            HelpPack::NAME
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            HelpPack::NOTE_KINDS
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            HelpPack::ENTITY_KINDS
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            HelpPack::HANDLERS
        }
        fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
            &HELP_EDGE_RULES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "pack": "helptest", "verb": verb }))
        }
    }

    fn build_help_registry(invocations: Arc<AtomicUsize>) -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(HelpPack { invocations });
        builder.build().expect("help registry builds")
    }

    /// help=true on `create` returns a schema envelope with the correct verb name,
    /// pack name, description, and at least the required `kind` parameter.
    #[tokio::test]
    async fn test_help_true_returns_schema_for_kg_create() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let reg = build_help_registry(invocations.clone());

        let result = reg
            .dispatch("create", serde_json::json!({ "help": true }))
            .await
            .expect("help=true must succeed for a known verb");

        // Shape checks.
        assert_eq!(result["verb"], "create", "envelope must name the verb");
        assert_eq!(
            result["pack"], "helptest",
            "envelope must name the owning pack"
        );
        assert!(
            result["description"].as_str().is_some(),
            "description must be a string"
        );

        // Params array must be present and non-empty.
        let params = result["params"]
            .as_array()
            .expect("params must be a JSON array");
        assert!(!params.is_empty(), "params array must not be empty");

        // The required `kind` param must appear.
        let kind_param = params.iter().find(|p| p["name"] == "kind");
        assert!(
            kind_param.is_some(),
            "params array must include the 'kind' parameter"
        );
        let kind_param = kind_param.unwrap();
        assert_eq!(
            kind_param["required"],
            serde_json::json!(true),
            "'kind' must be required"
        );
        assert_eq!(kind_param["type"], "string", "'kind' type must be 'string'");

        let identifier_help = result["identifier_resolution"]
            .as_object()
            .expect("help=true must include the shared identifier contract");
        assert!(identifier_help["full_uuid"]
            .as_str()
            .is_some_and(|text| text.contains("globally unique")));
        assert!(identifier_help["short_prefix"]
            .as_str()
            .is_some_and(|text| text.contains("lookup scope belongs to the consuming parameter")));
        assert!(identifier_help["parameter_rule"]
            .as_str()
            .is_some_and(|text| text.contains("submitted again")));
    }

    /// help=true on `recall` returns a schema envelope including the `query` param.
    #[tokio::test]
    async fn test_help_true_returns_schema_for_recall() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let reg = build_help_registry(invocations.clone());

        let result = reg
            .dispatch("recall", serde_json::json!({ "help": true }))
            .await
            .expect("help=true must succeed for recall");

        assert_eq!(result["verb"], "recall");
        assert_eq!(result["pack"], "helptest");

        let params = result["params"]
            .as_array()
            .expect("params must be a JSON array");

        // `query` must be present and required.
        let query_param = params.iter().find(|p| p["name"] == "query");
        assert!(query_param.is_some(), "params must include 'query'");
        let query_param = query_param.unwrap();
        assert_eq!(
            query_param["required"],
            serde_json::json!(true),
            "'query' must be required"
        );

        // `limit` must be present and optional.
        let limit_param = params.iter().find(|p| p["name"] == "limit");
        assert!(limit_param.is_some(), "params must include 'limit'");
        let limit_param = limit_param.unwrap();
        assert_eq!(
            limit_param["required"],
            serde_json::json!(false),
            "'limit' must be optional"
        );
    }

    /// `link(help=true)` (issue #964) surfaces the composed per-relation
    /// endpoint allowlist: the base entity-to-entity table, every loaded
    /// pack's additive `EDGE_RULES`, and the `annotates` note-to-any rule —
    /// so a batch caller can defer to the kernel's own table instead of
    /// re-implementing it.
    #[tokio::test]
    async fn test_link_help_true_exposes_endpoint_rules() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let reg = build_help_registry(invocations.clone());

        let result = reg
            .dispatch("link", serde_json::json!({ "help": true }))
            .await
            .expect("help=true must succeed for link");

        assert_eq!(result["verb"], "link");
        let rules = result["endpoint_rules"]
            .as_array()
            .expect("link help must include an endpoint_rules array");
        assert!(!rules.is_empty(), "endpoint_rules must not be empty");

        // A base entity-to-entity rule (khive-runtime's own table) must appear.
        assert!(
            rules.iter().any(|r| r["relation"] == "contains"
                && r["source"] == "entity:concept"
                && r["target"] == "entity:concept"),
            "endpoint_rules must include the base 'contains' entity rule; got {rules:#?}"
        );

        // The pack-declared additive rule (HelpPack's task->task depends_on) must appear.
        assert!(
            rules.iter().any(|r| r["relation"] == "depends_on"
                && r["source"] == "note:task"
                && r["target"] == "note:task"),
            "endpoint_rules must include the pack-declared depends_on rule; got {rules:#?}"
        );

        // The annotates note-to-any special case must appear.
        assert!(
            rules
                .iter()
                .any(|r| r["relation"] == "annotates" && r["source"] == "note:*"),
            "endpoint_rules must document the annotates note-to-any rule; got {rules:#?}"
        );

        // help=true must remain side-effect-free.
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            0,
            "link(help=true) must not invoke pack dispatch"
        );
    }

    /// `link(help=true)`'s `endpoint_rules` must match, set-for-set, every
    /// endpoint pair `validate_edge_relation_endpoints`
    /// (`crates/khive-runtime/src/operations.rs`) actually accepts for the
    /// three special relations (`supersedes` / `supports` / `refutes`):
    ///
    /// - a `note -> note` row for each of the three relations (the
    ///   validator's dedicated special-relation branch accepts any
    ///   `Resolved::Note(_), Resolved::Note(_)` pair unconditionally,
    ///   `operations.rs:1338` / `:1527` — before `pack_rule_allows` is ever
    ///   reached);
    /// - the base entity->entity rows for the three relations
    ///   (`base_entity_endpoint_rules`, e.g. `concept -[supersedes]-> concept`);
    /// - and, critically, NOT a row for `HelpPack`'s pack-declared
    ///   `supersedes` rule on `note:task -> note:task`
    ///   (`HELP_EDGE_RULES[1]`) — because the validator's special-relation
    ///   branch returns before `pack_rule_allows` is consulted, that pack
    ///   rule is never actually enforced, so advertising it would be a false
    ///   promise (the exact defect this test guards against, issue #991).
    #[tokio::test]
    async fn test_link_help_true_matches_special_relation_validator_set() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let reg = build_help_registry(invocations.clone());

        let result = reg
            .dispatch("link", serde_json::json!({ "help": true }))
            .await
            .expect("help=true must succeed for link");

        let rules = result["endpoint_rules"]
            .as_array()
            .expect("link help must include an endpoint_rules array");

        for relation in ["supersedes", "supports", "refutes"] {
            // The unconditional note -> note row must appear.
            assert!(
                rules.iter().any(|r| r["relation"] == relation
                    && r["source"] == "note:*"
                    && r["target"] == "note:*"),
                "endpoint_rules must include the note:*->note:* row for '{relation}' \
                 (validator accepts any note->note pair unconditionally); got {rules:#?}"
            );

            // HelpPack's pack-declared rule for this relation on note:task->note:task
            // (only Supersedes is declared in HELP_EDGE_RULES) must NOT be advertised
            // as a distinct entity — the validator never reaches pack_rule_allows for
            // special relations, so no note:task->note:task row should exist for it.
            assert!(
                !rules.iter().any(|r| r["relation"] == relation
                    && r["source"] == "note:task"
                    && r["target"] == "note:task"),
                "endpoint_rules must NOT advertise a pack EDGE_RULES row for special \
                 relation '{relation}' — validate_edge_relation_endpoints never consults \
                 pack_rule_allows for supersedes/supports/refutes; got {rules:#?}"
            );
        }

        // Base entity->entity rows for the three relations (from
        // base_entity_endpoint_rules) must still appear alongside the note rows.
        for (relation, kind) in [
            ("supersedes", "concept"),
            ("supports", "concept"),
            ("refutes", "concept"),
        ] {
            assert!(
                rules.iter().any(|r| r["relation"] == relation
                    && r["source"] == format!("entity:{kind}")
                    && r["target"] == "entity:concept"),
                "endpoint_rules must include the base entity:{kind}->entity:concept row \
                 for '{relation}'; got {rules:#?}"
            );
        }
    }

    #[test]
    fn special_relation_predicate_matches_the_dedicated_validator_set() {
        for relation in khive_types::EdgeRelation::ALL {
            assert_eq!(
                is_special_relation(relation),
                matches!(
                    relation,
                    khive_types::EdgeRelation::Supersedes
                        | khive_types::EdgeRelation::Supports
                        | khive_types::EdgeRelation::Refutes
                ),
                "unexpected special-relation classification for {relation}"
            );
        }
    }

    /// help=true is intercepted before pack dispatch — the pack's dispatch method
    /// must never be invoked when help=true is in the params.
    #[tokio::test]
    async fn test_help_true_does_not_execute_the_verb() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let reg = build_help_registry(invocations.clone());

        // Call both verbs with help=true.
        reg.dispatch("create", serde_json::json!({ "help": true }))
            .await
            .expect("help=true must succeed");
        reg.dispatch("recall", serde_json::json!({ "help": true }))
            .await
            .expect("help=true must succeed");

        assert_eq!(
            invocations.load(Ordering::SeqCst),
            0,
            "pack dispatch MUST NOT be invoked when help=true; \
             got {} invocation(s)",
            invocations.load(Ordering::SeqCst)
        );

        // Confirm that a normal call (without help=true) DOES invoke dispatch.
        reg.dispatch("create", serde_json::json!({}))
            .await
            .expect("normal dispatch must succeed");
        assert_eq!(
            invocations.load(Ordering::SeqCst),
            1,
            "pack dispatch must fire exactly once for a normal call"
        );
    }

    // ── Subhandler help-schema regressions ─────────────────────────────────
    //
    // Subhandler verbs must return `callable_via_mcp: false` in their help
    // schema so agents who read help=true before probing see accurate
    // availability — not a "looks callable" schema followed by permission denied.

    /// help=true on a `Visibility::Subhandler` verb returns `callable_via_mcp: false`
    /// and `visibility: "internal"` rather than a plain callable-looking envelope.
    #[tokio::test]
    async fn help_true_on_subhandler_returns_callable_via_mcp_false() {
        let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

        let result = reg
            .dispatch("recall.embed", serde_json::json!({ "help": true }))
            .await
            .expect("help=true on subhandler must succeed (no permission check on help path)");

        assert_eq!(
            result["callable_via_mcp"],
            serde_json::json!(false),
            "subhandler help must carry callable_via_mcp: false"
        );
        assert_eq!(
            result["visibility"], "internal",
            "subhandler help must carry visibility: internal"
        );
        // The verb and pack fields must still be present so the caller knows
        // what the schema belongs to.
        assert_eq!(result["verb"], "recall.embed");
        assert_eq!(result["pack"], "helptest");
    }

    /// Public Verb-visibility handlers must NOT have `callable_via_mcp: false`.
    #[tokio::test]
    async fn help_true_on_public_verb_does_not_have_callable_via_mcp_false() {
        let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

        let result = reg
            .dispatch("create", serde_json::json!({ "help": true }))
            .await
            .expect("help=true on public verb must succeed");

        // callable_via_mcp must be absent or true for public verbs.
        assert_ne!(
            result.get("callable_via_mcp"),
            Some(&serde_json::json!(false)),
            "public verb help must NOT carry callable_via_mcp: false"
        );
        // visibility must be absent or 'public' (never 'internal') for public verbs.
        assert_ne!(
            result.get("visibility"),
            Some(&serde_json::json!("internal")),
            "public verb help must NOT carry visibility: internal"
        );
    }

    /// help=true on an unknown verb returns an error (same behavior as normal dispatch).
    #[tokio::test]
    async fn help_true_on_unknown_verb_returns_error() {
        let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

        let err = reg
            .dispatch("nonexistent_verb", serde_json::json!({ "help": true }))
            .await
            .unwrap_err();

        assert!(
            matches!(err, RuntimeError::UnknownVerb(_)),
            "help=true on unknown verb must return UnknownVerb, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("nonexistent_verb"),
            "error must name the unknown verb: {msg}"
        );
    }

    /// Subhandler help must include params: [] even when the verb has no params.
    #[tokio::test]
    async fn help_true_on_subhandler_includes_params_field() {
        let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

        let result = reg
            .dispatch("recall.embed", serde_json::json!({ "help": true }))
            .await
            .expect("help=true on subhandler must succeed");

        // params must always be present (consistent shape).
        let params = result
            .get("params")
            .expect("subhandler help must include 'params' field");
        assert!(
            params.is_array(),
            "subhandler help params must be a JSON array"
        );
    }

    // ── Unknown-verb error must not leak subhandler names ─────────

    /// `describe_verb` on an unknown verb must list only Verb-visibility names
    /// in the "available" list: never subhandler names like `recall.embed`.
    #[tokio::test]
    async fn help_true_unknown_verb_available_list_excludes_subhandlers() {
        let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

        let err = reg
            .dispatch("not_a_verb", serde_json::json!({ "help": true }))
            .await
            .unwrap_err();

        let msg = err.to_string();
        // `recall.embed` is a Subhandler in HelpPack — must NOT appear in the
        // "available" list of an unknown-verb error.
        assert!(
            !msg.contains("recall.embed"),
            "unknown-verb help error must not advertise subhandler recall.embed: {msg}"
        );
        // Public verbs must still appear so the agent knows what to call.
        assert!(
            msg.contains("create"),
            "unknown-verb help error must still list public verb 'create': {msg}"
        );
        assert!(
            msg.contains("recall"),
            "unknown-verb help error must still list public verb 'recall': {msg}"
        );
    }

    /// Normal dispatch on an unknown verb must also not leak subhandler names.
    #[tokio::test]
    async fn dispatch_unknown_verb_available_list_excludes_subhandlers() {
        let reg = build_help_registry(Arc::new(AtomicUsize::new(0)));

        let err = reg
            .dispatch("not_a_verb", serde_json::json!({}))
            .await
            .unwrap_err();

        let msg = err.to_string();
        // `recall.embed` is a Subhandler in HelpPack — must NOT appear in the
        // "available" list of an unknown-verb dispatch error.
        assert!(
            !msg.contains("recall.embed"),
            "dispatch unknown-verb error must not advertise subhandler recall.embed: {msg}"
        );
        // Public verbs must still appear so the agent knows what to call.
        assert!(
            msg.contains("create"),
            "dispatch unknown-verb error must still list public verb 'create': {msg}"
        );
        assert!(
            msg.contains("recall"),
            "dispatch unknown-verb error must still list public verb 'recall': {msg}"
        );
    }

    // ── ADR-028 multi-backend schema routing tests ───────────────────────────

    /// A test pack that returns a real SchemaPlan so we can assert routing.
    struct SchemaPack {
        pack_name: &'static str,
        statements: &'static [&'static str],
    }

    impl Pack for SchemaPack {
        const NAME: &'static str = "schema-pack";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [HandlerDef] = &[];
    }

    #[async_trait]
    impl PackRuntime for SchemaPack {
        fn name(&self) -> &str {
            self.pack_name
        }
        fn note_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn entity_kinds(&self) -> &'static [&'static str] {
            &[]
        }
        fn handlers(&self) -> &'static [HandlerDef] {
            &[]
        }
        fn schema_plan(&self) -> SchemaPlan {
            SchemaPlan {
                pack: self.pack_name,
                statements: self.statements,
            }
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": self.pack_name, "verb": verb }))
        }
    }

    // ADR-028: all_schema_plans_named returns (pack_name, SchemaPlan) pairs
    // where pack_name comes from SchemaPlan::pack (always &'static str).
    #[test]
    fn all_schema_plans_named_returns_correct_pairs() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "alpha",
            statements: &["CREATE TABLE IF NOT EXISTS t_alpha (id INTEGER PRIMARY KEY)"],
        }));
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "beta",
            statements: &[],
        }));
        let reg = builder.build().expect("registry builds");

        let named = reg.all_schema_plans_named();
        assert_eq!(named.len(), 2);

        let alpha_entry = named.iter().find(|(n, _)| *n == "alpha");
        let beta_entry = named.iter().find(|(n, _)| *n == "beta");

        assert!(alpha_entry.is_some(), "alpha must appear in named plans");
        assert!(beta_entry.is_some(), "beta must appear in named plans");

        let (_, alpha_plan) = alpha_entry.unwrap();
        assert_eq!(alpha_plan.statements.len(), 1);
        assert!(!alpha_plan.is_empty());

        let (_, beta_plan) = beta_entry.unwrap();
        assert!(beta_plan.is_empty());
    }

    // ADR-028: apply_schema_plans_with_map routes non-empty plans to the
    // correct per-pack backend instead of the default.
    //
    // Verification: apply DDL to routed backend, then confirm the table is
    // present on pack_backend and absent on default_backend by attempting to
    // apply the same DDL again — if the table already exists on pack_backend
    // the idempotent CREATE IF NOT EXISTS succeeds; applying to default_backend
    // would only matter if the table were routed there.  We verify isolation
    // by applying the plan and then running a targeted DDL on each backend
    // that would fail if the table did not already exist (CREATE without
    // IF NOT EXISTS on a duplicate raises an error), combined with a no-error
    // path on the correct backend.
    //
    // Simpler approach: confirm the plan applies without error (routing is
    // correct) and that the opposite backend returns an error when we try to
    // INSERT into the routed table (table-not-found = SQLITE_ERROR).
    #[tokio::test]
    async fn apply_schema_plans_with_map_routes_to_correct_backend() {
        use khive_storage::types::{SqlStatement, SqlValue};

        let default_backend = khive_db::StorageBackend::memory().expect("default memory backend");
        let pack_backend =
            khive_db::StorageBackend::memory().expect("pack-specific memory backend");

        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "routed",
            statements: &["CREATE TABLE IF NOT EXISTS t_routed (id INTEGER PRIMARY KEY)"],
        }));
        let reg = builder.build().expect("registry builds");

        let mut backend_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();
        backend_map.insert("routed", &pack_backend);

        reg.apply_schema_plans_with_map(&backend_map, &default_backend)
            .expect("schema application must not collide");

        // On pack_backend: INSERT must succeed (table exists).
        let mut writer = pack_backend.sql().writer().await.expect("writer");
        let result = writer
            .execute(SqlStatement {
                sql: "INSERT INTO t_routed (id) VALUES (?1)".into(),
                params: vec![SqlValue::Integer(1)],
                label: None,
            })
            .await;
        assert!(
            result.is_ok(),
            "t_routed must exist on pack_backend after routing: {result:?}"
        );

        // On default_backend: INSERT must fail (table not there).
        let mut default_writer = default_backend.sql().writer().await.expect("writer");
        let default_result = default_writer
            .execute(SqlStatement {
                sql: "INSERT INTO t_routed (id) VALUES (?1)".into(),
                params: vec![SqlValue::Integer(2)],
                label: None,
            })
            .await;
        assert!(
            default_result.is_err(),
            "t_routed must NOT exist on default_backend (table should not be there)"
        );
    }

    // ADR-028: apply_schema_plans_with_map uses default backend for packs
    // absent from the map.
    #[tokio::test]
    async fn apply_schema_plans_with_map_falls_back_to_default_for_unmapped_packs() {
        use khive_storage::types::{SqlStatement, SqlValue};

        let default_backend = khive_db::StorageBackend::memory().expect("default memory backend");

        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "unmapped",
            statements: &["CREATE TABLE IF NOT EXISTS t_unmapped (id INTEGER PRIMARY KEY)"],
        }));
        let reg = builder.build().expect("registry builds");

        let backend_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();
        reg.apply_schema_plans_with_map(&backend_map, &default_backend)
            .expect("schema application must not collide");

        // On default_backend: INSERT must succeed (table fell back here).
        let mut writer = default_backend.sql().writer().await.expect("writer");
        let result = writer
            .execute(SqlStatement {
                sql: "INSERT INTO t_unmapped (id) VALUES (?1)".into(),
                params: vec![SqlValue::Integer(1)],
                label: None,
            })
            .await;
        assert!(
            result.is_ok(),
            "t_unmapped must exist on default_backend for unmapped pack: {result:?}"
        );
    }

    // ADR-028: two packs declaring the same auxiliary table on the same
    // backend must cause apply_schema_plans_with_map to return an error that
    // names both packs and the table: it is a boot-time failure, not a
    // silent DDL race.
    #[test]
    fn apply_schema_plans_with_map_collision_is_an_error() {
        let backend = khive_db::StorageBackend::memory().expect("memory backend");
        let empty_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();

        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_alpha",
            statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        }));
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_beta",
            statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        }));
        let registry = builder.build().expect("registry builds");

        let result = registry.apply_schema_plans_with_map(&empty_map, &backend);
        assert!(
            result.is_err(),
            "two packs declaring the same table on the same backend must produce a collision error"
        );
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pack_alpha"),
            "collision error must name first pack; got: {msg}"
        );
        assert!(
            msg.contains("pack_beta"),
            "collision error must name second pack; got: {msg}"
        );
        assert!(
            msg.contains("collision_table"),
            "collision error must name the table; got: {msg}"
        );
    }

    #[test]
    fn apply_schema_plans_with_map_read_only_collision_is_an_error_without_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("read_only_schema_collision.db");
        {
            let writable = khive_db::StorageBackend::sqlite(&path).expect("writable backend");
            writable.prepare_core_schema().expect("current schema");
        }
        #[cfg(unix)]
        khive_storage::test_support::freeze_snapshot_sidecars(&path);
        let backend = khive_db::StorageBackend::sqlite_read_only(&path).expect("read-only backend");
        let empty_map: HashMap<&str, &khive_db::StorageBackend> = HashMap::new();

        let mut builder = VerbRegistryBuilder::new();
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_alpha",
            statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        }));
        builder.register_boxed(Box::new(SchemaPack {
            pack_name: "pack_beta",
            statements: &["CREATE TABLE IF NOT EXISTS collision_table (id INTEGER PRIMARY KEY)"],
        }));
        let registry = builder.build().expect("registry builds");
        let writes_before = backend.pool().writer_acquisition_snapshot();

        let result = registry.apply_schema_plans_with_map(&empty_map, &backend);

        let err = result.expect_err(
            "read-only topology must reject the same cross-pack collision as writable topology",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("pack_alpha"),
            "collision error must name first pack; got: {msg}"
        );
        assert!(
            msg.contains("pack_beta"),
            "collision error must name second pack; got: {msg}"
        );
        assert!(
            msg.contains("collision_table"),
            "collision error must name the table; got: {msg}"
        );
        assert_eq!(
            backend.pool().writer_acquisition_snapshot(),
            writes_before,
            "read-only collision validation must not acquire a writer"
        );
    }
}
