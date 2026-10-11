use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use khive_storage::EventView;
use serde_json::Value;

use crate::context::ContextContributor;
use crate::operations::LinkSpec;
use crate::runtime::NamespaceToken;
use crate::validation::ValidationRule;
use crate::{KhiveRuntime, RuntimeError};

use super::{
    ChannelIngestCapability, EdgeEndpointRule, EntityTypeDef, HandlerDef, NoteEmbeddingPolicySpec,
    NoteKindSpec, PackColumnAddition, VerbRegistry,
};
#[cfg(doc)]
use super::{PackFactory, PackRegistry};

/// Pack-auxiliary schema plan.
///
/// Declares `CREATE TABLE IF NOT EXISTS` statements for pack-owned tables that
/// are NOT part of the core substrate schema (entities, notes, edges, events).
/// Applied at boot via `StorageBackend::apply_pack_ddl_statements_with_columns`,
/// together with [`PackRuntime::schema_column_additions`].
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

    /// Optional instance-owned state for host work outside verb dispatch.
    /// Return the same shared state used by this pack's handlers. The host
    /// owns task startup and shutdown; this accessor must not start work.
    fn host_state(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// Validate this pack instance's configuration before it can execute.
    /// Metadata-only construction does not activate packs.
    fn validate_config(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Note kinds this pack owns — must equal `<Self as Pack>::NOTE_KINDS`.
    fn note_kinds(&self) -> &'static [&'static str];

    /// Entity kinds this pack owns — must equal `<Self as Pack>::ENTITY_KINDS`.
    fn entity_kinds(&self) -> &'static [&'static str];

    /// Brain profile consumer kinds this pack requests — must equal
    /// `<Self as Pack>::BRAIN_CONSUMER_KINDS`.
    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Trusted in-process section feedback after the calling pack has validated
    /// its own target. This is not a registered handler or a wire entry point.
    async fn apply_profile_section_feedback(
        &self,
        _token: &NamespaceToken,
        _profile_id: &str,
        _section_signals: Value,
        _target_attribution: Option<String>,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "pack {:?} does not support profile section feedback",
            self.name()
        )))
    }

    /// Handlers this pack registers — must equal `<Self as Pack>::HANDLERS`.
    fn handlers(&self) -> &'static [HandlerDef];

    /// Optional canonical input schema owned by the pack; ParamDefs remain available.
    fn input_schema(&self, _verb: &str) -> Option<Value> {
        None
    }

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

    /// Per-kind write-time embedding policy; unlisted kinds use every model.
    fn note_embedding_policies(&self) -> &'static [NoteEmbeddingPolicySpec] {
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

    /// Optional context source, discovered without invoking contribution or dispatch.
    ///
    /// Return the instance-owned capability. Packs without context support keep the default.
    /// See `docs/api/pack.md#context_contributor` for ownership and score contracts.
    fn context_contributor(&self) -> Option<Arc<dyn ContextContributor>> {
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

    /// Nullable-column upgrades for this pack's auxiliary tables.
    ///
    /// Must equal `Pack::SCHEMA_COLUMN_ADDITIONS`. The backend validates and
    /// adds missing columns on existing tables before applying the full schema
    /// plan, then validates every declared column. Both steps share the plan's
    /// transaction. Defaults to empty for packs with no auxiliary upgrades.
    fn schema_column_additions(&self) -> &'static [PackColumnAddition] {
        &[]
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

    /// Install a backend-matched note-search ANN candidate source. The
    /// memory pack supplies it after registration; packs without a matching
    /// graph leave the runtime's exact vector-store route in place.
    fn register_note_search_ann_provider(&self, _runtime: &KhiveRuntime) {}

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

    fn mounted_namespace(&self) -> Option<&str> {
        None
    }

    /// Advisory owned catalog only: no storage, process, gate, or audit work.
    fn mounted_catalog_snapshot(&self) -> Vec<crate::mounted_verb::MountedVerb> {
        Vec::new()
    }

    async fn mounted_catalog(&self) -> Result<Vec<crate::mounted_verb::MountedVerb>, RuntimeError> {
        Ok(Vec::new())
    }

    async fn dispatch_mounted(
        &self,
        _definition: &crate::mounted_verb::MountedVerb,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.dispatch(verb, params, registry, token).await
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
/// verbs. Shared `create`, note `update`, entity `update`, and `link` calls
/// flow through this trait when an endpoint kind has an owning pack hook.
///
/// A hook that still overrides the removed sequencing method does not compile, which is the point
/// of the move: an implementor cannot replace the validator by replacing the sequence, because
/// there is no sequence on this trait to replace.
///
/// ```compile_fail
/// use async_trait::async_trait;
/// use khive_runtime::{KhiveRuntime, KindHook, NamespaceToken, RuntimeError};
/// use serde_json::Value;
///
/// #[derive(Debug)]
/// struct Sequencing;
///
/// #[async_trait]
/// impl KindHook for Sequencing {
///     async fn prepare_create(
///         &self,
///         _runtime: &KhiveRuntime,
///         _args: &mut Value,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
///
///     async fn after_create(
///         &self,
///         _runtime: &KhiveRuntime,
///         _id: uuid::Uuid,
///         _args: &Value,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
///
///     async fn prepare_note_update(
///         &self,
///         _runtime: &KhiveRuntime,
///         _token: &NamespaceToken,
///         _note: &khive_storage::Note,
///         _args: &mut Value,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
/// }
/// ```
///
/// The companion below is the control, and it is what makes the arm above mean anything: a
/// `compile_fail` doctest passes when the code fails to compile for ANY reason, including a stale
/// import or a renamed type. This one is structurally identical and overrides the two halves a pack
/// is meant to implement, so it must compile — if it stops compiling, the arm above has stopped
/// testing the method and is passing on the scaffolding instead.
///
/// ```
/// use async_trait::async_trait;
/// use khive_runtime::{KhiveRuntime, KindHook, NamespaceToken, RuntimeError};
/// use serde_json::Value;
///
/// #[derive(Debug)]
/// struct Halves;
///
/// #[async_trait]
/// impl KindHook for Halves {
///     async fn prepare_create(
///         &self,
///         _runtime: &KhiveRuntime,
///         _args: &mut Value,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
///
///     async fn after_create(
///         &self,
///         _runtime: &KhiveRuntime,
///         _id: uuid::Uuid,
///         _args: &Value,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
///
///     async fn normalize_note_update(
///         &self,
///         _runtime: &KhiveRuntime,
///         _token: &NamespaceToken,
///         _note: &khive_storage::Note,
///         _args: &mut Value,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
///
///     async fn validate_note_update(
///         &self,
///         _runtime: &KhiveRuntime,
///         _token: &NamespaceToken,
///         _note: &khive_storage::Note,
///         _properties: Option<&Value>,
///     ) -> Result<(), RuntimeError> {
///         Ok(())
///     }
/// }
/// ```
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
    /// best-effort side effects. The default does nothing.
    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: uuid::Uuid,
        _args: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Validate an approved AddEntity draft before preparing domain writes.
    /// The draft kind is canonical.
    /// This must not mutate storage or normalize the approved draft. The default
    /// accepts it. This separate seam never invokes shared-create lifecycle
    /// hooks and does not apply to AddNote; see `validate_proposal_note` below
    /// for that route.
    fn validate_proposal_entity(
        &self,
        _entity: &khive_types::EntityDraft,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Validate an `AddNote` draft on the proposal-note route, analogous to
    /// [`Self::validate_proposal_entity`] but for notes. The kg pack's
    /// proposal route calls this against the same immutable changeset at two
    /// points: once when a new `propose` call is accepted, and again when an
    /// approved proposal is applied, so a kind that refuses shared creation
    /// is not bypassed by proposing the same creation instead. The draft's
    /// kind is the owning pack's canonical spelling. This must not mutate
    /// storage or normalize the draft; it only accepts or refuses. The
    /// default accepts it.
    fn validate_proposal_note(&self, _note: &khive_types::NoteDraft) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Normalize caller-facing note-update fields before validation runs.
    ///
    /// Override this when a kind-owning pack's caller-facing note fields
    /// mirror owned properties and must be changed together (for example, a
    /// task's searchable `content` and `properties.description`). Validation
    /// is not this method's job: the registry runs
    /// [`Self::validate_note_update`] after this method returns, regardless of
    /// what this method did. The default does nothing.
    async fn normalize_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Validate a shared note-property update before storage is mutated.
    ///
    /// The default accepts the update. Kind-owning packs override this when a
    /// property has invariants that generic CRUD cannot know about (for
    /// example, GTD task dependency acyclicity). This always runs after
    /// [`Self::normalize_note_update`], because
    /// [`VerbRegistry::prepare_note_update_policy`] calls them in that order.
    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        _properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Describe graph changes coupled to a validated note patch, without writing.
    ///
    /// The dispatcher calls this only after normalization, kind validation, and
    /// preparation of the note's guarded write. The runtime prepares these typed
    /// effects and commits them with that write in one atomic unit. Implementors
    /// must derive effects from this exact snapshot and patch; omitted or unchanged
    /// owned fields should return no effects. This is not an after-update hook.
    async fn note_update_effects(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        _patch: &crate::curation::NotePatch,
    ) -> Result<Vec<NoteUpdateEffect>, RuntimeError> {
        Ok(Vec::new())
    }

    /// Optional top-level properties whose explicit null update deletes the
    /// stored key after the shared merge. Omission still preserves the key.
    /// The default changes no property semantics. This policy is returned only
    /// after normalization and validation have accepted the update.
    fn note_update_null_clearing_properties(&self) -> &'static [&'static str] {
        &[]
    }

    /// Validate a shared entity-property update before storage is mutated.
    ///
    /// Runs after the caller's patch has been merged into the entity's
    /// stored properties, so `properties` reflects the resulting record
    /// rather than the raw patch — the invariant this validates (e.g. "a
    /// required key must be present and typed") is a claim about the
    /// record, not about what one caller happened to send. This is
    /// deliberately NOT a re-run of `prepare_create`: a `prepare_create`
    /// body may also enforce create-shape requirements (an argument the
    /// caller must supply at create time) that a partial update
    /// legitimately omits, and re-running it would reject valid updates
    /// with an error message written for create.
    ///
    /// The default accepts the update. Kind-owning packs override this when
    /// a `prepare_create` invariant must also hold after a generic
    /// `update`, sharing one predicate between both methods the way
    /// [`validate_note_update`](Self::validate_note_update)'s implementors
    /// already do for notes.
    async fn validate_entity_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _entity: &khive_storage::Entity,
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

/// A kind-owned graph mutation committed with its note update. SQL and deferred
/// callbacks are deliberately not part of this interface.
#[derive(Clone, Debug)]
pub enum NoteUpdateEffect {
    /// Create or explicitly resurrect an outgoing edge using normal link guards.
    Link(LinkSpec),
    /// Soft-delete one existing outgoing edge by ID, guarded by this snapshot.
    DeleteEdge(khive_storage::types::Edge),
    /// Preserve a live outgoing edge and assert its identity and endpoints at
    /// commit time without changing its ID, timestamps, weight, or metadata.
    AssertLink(khive_storage::types::Edge),
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

    /// Verbs that change this pack's private records, named as examples when
    /// a generic verb refuses one of them (ADR-061 Amendment 1).
    ///
    /// The refusal names the owning pack either way. A pack that returns
    /// nothing is named without examples rather than borrowing another pack's.
    fn private_record_verbs(&self) -> &'static [&'static str] {
        &[]
    }
}
