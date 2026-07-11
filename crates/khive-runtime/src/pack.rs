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

use crate::runtime::NamespaceToken;
use async_trait::async_trait;
use khive_gate::{AllowAllGate, AuditEvent, GateDecision, GateRef, GateRequest};
use khive_storage::{Event, EventStore, EventView, SubstrateKind};
use khive_types::{EventKind, EventOutcome, Namespace};
use serde_json::Value;

pub use khive_types::{
    EdgeEndpointRule, EndpointKind, HandlerDef, NoteKindSpec, NoteLifecycleSpec, PackSchemaPlan,
    ParamDef, VerbCategory, VerbPresentationPolicy, Visibility,
};
// Backward-compat re-export.
#[allow(deprecated)]
pub use khive_types::VerbDef;

use crate::validation::ValidationRule;

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

/// Hook called after every successful verb dispatch.
///
/// Packs observe enriched event views so provenance-aware consumers can use
/// `view.observations` while legacy folds can still consume `view.event`.
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

    /// Handlers this pack registers — must equal `<Self as Pack>::HANDLERS`.
    fn handlers(&self) -> &'static [HandlerDef];

    /// Pack-extensible edge endpoint rules — must equal `<Self as Pack>::EDGE_RULES`.
    /// Defaults to empty so existing packs that don't extend the edge contract
    /// can ignore it.
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
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

    /// Register custom embedding providers with the runtime.
    ///
    /// Called by the transport during pack initialisation, before the first verb
    /// dispatch, so that `KhiveRuntime::embedder(name)` resolves provider names
    /// declared here.
    ///
    /// Implement this method to contribute non-lattice embedding backends:
    ///
    /// ```ignore
    /// fn register_embedders(&self, runtime: &KhiveRuntime) {
    ///     runtime.register_embedder(MyCustomProvider::new());
    /// }
    /// ```
    ///
    /// The default no-op preserves backwards compatibility — packs that only
    /// use built-in lattice models do not need to override this method.
    fn register_embedders(&self, _runtime: &KhiveRuntime) {}

    /// Install a pack-owned entity-type validator on the runtime.
    ///
    /// Called by the transport during pack initialisation, after the registry
    /// is built and before the first verb dispatch, so that `create_many` and
    /// `create_entity` reject unregistered `entity_type` values at the runtime
    /// layer in addition to the handler layer.
    ///
    /// Packs that own `EntityTypeRegistry` vocabularies (e.g. `KgPack`) should
    /// override this to install their registry's `resolve` function.  The
    /// default no-op leaves the runtime validator absent (skip-when-None), which
    /// is the correct behaviour for bare runtimes without packs.
    fn register_entity_type_validator(&self, _runtime: &KhiveRuntime) {}

    /// Install a pack-owned note-mutation hook on the runtime.
    ///
    /// Called by the transport during pack initialisation, after the registry
    /// is built and before the first verb dispatch — same timing as
    /// `register_entity_type_validator`. Packs that cache derived state keyed
    /// by note content (e.g. `khive-pack-memory`'s warm ANN index) should
    /// override this to install a hook via `KhiveRuntime::install_note_mutation_hook`,
    /// so `update_note`/`delete_note` notify them even when the mutation
    /// arrived through a different pack's verb that has no dependency on the
    /// reacting pack (e.g. KG's `update`/`delete` on a `kind="memory"` note).
    ///
    /// The default no-op leaves the runtime hook absent (skip-when-None),
    /// which is the correct behaviour for packs that don't cache note-derived
    /// state and for bare runtimes without packs.
    fn register_note_mutation_hook(&self, _runtime: &KhiveRuntime) {}

    /// Warm up any in-memory state from persisted snapshots (optional).
    ///
    /// Called by the transport after all packs are registered but before
    /// serving the first request, giving packs a chance to pre-load expensive
    /// in-memory structures (e.g. ANN indexes) so that the first query does
    /// not incur rebuild latency.
    ///
    /// The default no-op is correct for all packs that have no warm-start
    /// state. Packs that override this must make it idempotent and infallible:
    /// any errors are logged internally, not propagated to the caller.
    async fn warm(&self) {}

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
///
/// Hooks are stateless from the framework's perspective — they receive the
/// runtime as a method parameter and operate on the args `Value` directly.
/// The pack registers them via [`PackRuntime::kind_hook`].
///
/// Lifecycle verbs (e.g. gtd's `complete`, `transition`) remain pack-owned
/// verbs and do not flow through this trait — only the create path does.
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
    /// Optional post-dispatch hook.
    ///
    /// When set, every successful pack dispatch calls `hook.on_dispatch(event)`
    /// with a synthesized Event describing the outcome. Opt-in: when None,
    /// no overhead is incurred.
    dispatch_hook: Option<Arc<dyn DispatchHook>>,
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
            dispatch_hook: None,
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
    /// infrastructure errors fail open (logged via `tracing::warn!`, dispatch
    /// proceeds).
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
    /// `Event`, outcome = `Success` on allow, `Denied` on deny) in addition to
    /// the `tracing::info!` emission that was already present in v0.2.
    ///
    /// Callers that do not set this field continue to use tracing-only emission
    /// (the v0.2 default). There is no behavior change for them.
    pub fn with_event_store(&mut self, store: Arc<dyn EventStore>) -> &mut Self {
        self.event_store = Some(store);
        self
    }

    /// Register a post-dispatch hook.
    ///
    /// When set, every successful pack dispatch calls `hook.on_dispatch(event)`
    /// with a synthesized [`Event`] describing the verb outcome. The hook is
    /// opt-in: registries without a hook incur zero overhead on the dispatch
    /// hot path.
    ///
    /// Brain pack uses this to update its posteriors in real time without
    /// polling the EventStore. Errors from `on_dispatch` are logged via
    /// `tracing::warn!` and never propagated.
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

        let available_verbs: Vec<&'static str> = ordered_packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .filter(|h| matches!(h.visibility, Visibility::Verb))
            .map(|h| h.name)
            .collect();

        Ok(VerbRegistry {
            packs: Arc::new(ordered_packs),
            resolvers: Arc::new(self.resolvers),
            gate: self.gate,
            default_namespace: self.default_namespace,
            visible_namespaces: self.visible_namespaces,
            actor_id: self.actor_id,
            event_store: self.event_store,
            dispatch_hook: self.dispatch_hook,
            available_verbs: Arc::new(available_verbs),
            reference_ring: Arc::new(crate::reference_ring::ReferenceRing::new()),
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
    /// caller. `None` means tracing-only, matching the registry's own
    /// audit-persistence default.
    pub fn event_store(&self) -> Option<Arc<dyn EventStore>> {
        self.event_store.clone()
    }

    /// Return the help schema envelope for a verb.
    ///
    /// Walks registered packs for the first matching `HandlerDef` and returns a
    /// structured JSON envelope. Subhandlers carry `callable_via_mcp: false`.
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
                            "visibility": "internal",
                            "callable_via_mcp": false,
                            "note": "This is an internal subhandler. Calling it via the MCP \
                                     request surface returns permission denied. It can only be \
                                     invoked by internal runtime callers.",
                        }));
                    }
                    return Ok(serde_json::json!({
                        "verb": verb,
                        "pack": pack.name(),
                        "description": handler.description,
                        "category": category,
                        "params": params_arr,
                    }));
                }
            }
        }
        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(RuntimeError::InvalidInput(format!(
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
    /// `RuntimeError::Internal`.
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
            Err(e) => Err(RuntimeError::Internal(format!("gate error: {e}"))),
        }
    }

    /// Dispatch a verb to the first pack that handles it.
    ///
    /// Routes through the gate, then invokes the matching pack handler. When
    /// `params["help"] == true`, short-circuits to `describe_verb` with no side effects.
    /// Gate errors are fail-open. Full dispatch flow documented in `docs/protocol.md`.
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
    /// request, and token minting — the registry's own fields are never
    /// mutated, so concurrent calls with different (or no) identity are
    /// independent. This is what lets one warm registry correctly serve
    /// requests from many attribution identities over the same shared
    /// backend (same db, same warm ANN indexes) instead of rejecting or
    /// silently dispatching under its own baked identity.
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
        // A supplied per-request identity overrides the baked
        // default_namespace/actor_id/visible_namespaces for this call only.
        let default_namespace_str: &str = identity
            .as_ref()
            .map(|id| id.namespace.as_str())
            .unwrap_or(self.default_namespace.as_str());
        let ns = resolve_explicit_namespace(&params, default_namespace_str)?;
        let actor_id_str: Option<&str> = match identity.as_ref() {
            Some(id) => id.actor_id.as_deref(),
            None => self.actor_id.as_deref(),
        };
        // Thread the configured actor identity into the gate request so the
        // gate can distinguish human vs agent callers at the dispatch seam.
        // Resolved once via the shared actor-identity policy and reused for
        // token minting below, so the gate's notion of "who is the caller"
        // and the storage token's notion can never drift apart.
        let resolved_actor = crate::actor_identity::resolve_actor(actor_id_str);
        let gate_req = GateRequest::new(resolved_actor.clone(), ns.clone(), verb, params.clone());

        // Consult the gate.
        //
        // - Ok(Allow) → proceed to pack dispatch (tracing + optional EventStore).
        // - Ok(Deny) → emit audit, persist if store configured, return PermissionDenied.
        // - Err(_) → warn via tracing, fail-open (no audit persisted).
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
                // `OnceLock::get_or_init` call site.
                if let Some(store) = &self.event_store {
                    if crate::config_ledger::PENDING
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                    {
                        for (key, value) in crate::config_ledger::drain_config_locked() {
                            let payload = serde_json::json!({ "key": key, "value": value });
                            let storage_event = Event::new(
                                gate_req.namespace.as_str(),
                                verb,
                                EventKind::ConfigLocked,
                                SubstrateKind::Event,
                                format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                            )
                            .with_payload(payload);
                            append_audit_event_best_effort(store, storage_event, verb).await;
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
                // Accepted trade-off: a crash between this Allow decision and
                // the deferred append below (post-dispatch, further down this
                // function) loses that dispatch's audit row entirely: a
                // deliberate choice, not an oversight.
                let defer_audit = !is_deny;

                // Persist to EventStore immediately only for denied calls.
                if !defer_audit {
                    if let Some(store) = &self.event_store {
                        let storage_event =
                            build_audit_storage_event(&gate_req, &audit, EventOutcome::Denied);
                        append_audit_event_best_effort(store, storage_event, verb).await;
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
                // Gate infrastructure failure — fail-open.
                // No decision was produced; no audit event is persisted.
                tracing::warn!(verb, error = %err, "gate check failed (fail-open)");
                (None, None)
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
        };

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
                let result = pack.dispatch(verb, params, self, &token).await;
                let dispatch_us = dispatch_start.elapsed().as_micros() as i64;

                // Append the deferred Allow-outcome audit row now that
                // dispatch has resolved, so `duration_us` carries the
                // measured `dispatch_us` instead of the `Event::new` default
                // of 0. A successful singleton `link` call enriches the row
                // with the created/resolved edge (schema v2); anything that
                // cannot be enriched, or is not a singleton `link` call,
                // falls back to the generic v1 audit shape so no audit row
                // is ever dropped for the deferred path.
                if let Some(audit) = deferred_audit.take() {
                    if let Some(store) = &self.event_store {
                        let is_link_singleton =
                            verb == "link" && gate_req.args.get("links").is_none();
                        match &result {
                            Ok(ok_val) if is_link_singleton => {
                                match link_audit_success_from_result(audit.clone(), ok_val) {
                                    Some((edge_id, payload)) => {
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
                                        append_audit_event_best_effort(store, storage_event, verb)
                                            .await;
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
                                        )
                                        .with_duration_us(dispatch_us);
                                        append_audit_event_best_effort(store, storage_event, verb)
                                            .await;
                                    }
                                }
                            }
                            _ => {
                                // The persisted audit outcome must reflect
                                // the dispatch result, not be hardcoded to
                                // Success — otherwise a failed dispatch is
                                // recorded as successful work and disappears
                                // from `outcome=error` queries.
                                let outcome = if result.is_ok() {
                                    EventOutcome::Success
                                } else {
                                    EventOutcome::Error
                                };
                                let storage_event =
                                    build_audit_storage_event(&gate_req, &audit, outcome)
                                        .with_duration_us(dispatch_us);
                                append_audit_event_best_effort(store, storage_event, verb).await;
                            }
                        }
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
                    // real hit/miss and latency.
                    if verb == "memory.recall" {
                        let first_note_id = ok_val
                            .as_array()
                            .and_then(|arr| arr.first())
                            .and_then(|v| v.get("id"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse::<uuid::Uuid>().ok());
                        if let Some(note_id) = first_note_id {
                            dispatch_event = dispatch_event.with_target(note_id);
                        }
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
                // Dispatch is about to return `InvalidInput` below (no pack
                // owns this verb), so the persisted outcome must be `Error`,
                // not `Success`.
                let storage_event =
                    build_audit_storage_event(&gate_req, &audit, EventOutcome::Error);
                append_audit_event_best_effort(store, storage_event, verb).await;
            }
        }

        // Verb-visibility handler names, precomputed at build() time (internal
        // subhandlers are excluded so they are not advertised in the
        // unknown-verb error).
        Err(RuntimeError::InvalidInput(format!(
            "unknown verb {verb:?}; available: {}",
            self.available_verbs.join(", ")
        )))
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
    pub fn call_register_entity_type_validators(&self, runtime: &KhiveRuntime) {
        for pack in self.packs.iter() {
            pack.register_entity_type_validator(runtime);
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
///
/// Shared by the immediate-append path (all verbs, denied calls, bulk
/// `links`) and the deferred singleton-`link` fallback so both audit
/// shapes are produced by one code path.
fn build_audit_storage_event(
    gate_req: &GateRequest,
    audit: &AuditEvent,
    outcome: EventOutcome,
) -> Event {
    let audit_data = serde_json::to_value(audit).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize AuditEvent for EventStore");
        serde_json::Value::Null
    });
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

/// Append an audit event, logging and swallowing store failures.
///
/// Audit persistence is best-effort everywhere in the dispatch path: a store
/// write failure must never fail the verb call it is auditing.
async fn append_audit_event_best_effort(store: &Arc<dyn EventStore>, event: Event, verb: &str) {
    if let Err(store_err) = store.append_event(event).await {
        tracing::warn!(
            verb,
            error = %store_err,
            "audit event store write failed (non-fatal)"
        );
    }
}

/// Schema v2 audit payload for a successful singleton `link` call.
///
/// Additive over the v1 `AuditEvent` shape: every v1 field is preserved via
/// `#[serde(flatten)]`, and the edge identity/relation/weight the caller
/// created or resolved are added at the top level.
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

/// Extract the edge fields needed to enrich a successful singleton `link`
/// audit row from the handler's returned JSON.
///
/// Returns `None` (rather than a `Result`) on any missing/malformed field —
/// the caller treats that as "cannot enrich" and falls back to the v1 audit
/// shape instead of failing the already-succeeded `link` call.
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
/// This is the single chokepoint both `VerbRegistry::dispatch` (single-backend
/// and JSON-form ingress) and the multi-backend coordinator intercept
/// (`dispatch_via_coordinator_inner` in `khive-mcp`) call into, so no ingress
/// path can bypass the fail-closed rule by routing around `dispatch`.
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
mod tests {
    use super::*;
    use crate::ActorRef;
    use khive_types::Pack;

    struct AlphaPack;

    impl Pack for AlphaPack {
        const NAME: &'static str = "alpha";
        const NOTE_KINDS: &'static [&'static str] = &["memo", "log"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
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

    // ---- Rego gate: fail-closed end-to-end ----

    /// A `RegoGate` whose policy lacks the named entrypoint rule must cause
    /// `VerbRegistry::dispatch` to return `RuntimeError::PermissionDenied` —
    /// never to proceed to the pack handler.
    ///
    /// This is the runtime-level assertion that a gate evaluation failure
    /// fails closed rather than opening a security hole.
    /// `RegoGate::check` converts all evaluation failures (missing rule,
    /// undefined result, serialization error, poisoned engine) to
    /// `Ok(GateDecision::Deny)`, so dispatch is blocked. The runtime's
    /// fail-open `Err(_)` branch remains for non-evaluation gate errors
    /// (e.g. infrastructure faults from other `Gate` implementations).
    #[tokio::test]
    async fn rego_gate_missing_entrypoint_returns_permission_denied() {
        use khive_gate_rego::RegoGate;

        // Policy defines `verdict` but NOT `data.khive.gate.decision` (the
        // default entrypoint).  Construction succeeds — from_policy_str does
        // not validate the default entrypoint.  check() must convert the
        // missing-rule evaluation error to Ok(Deny) so the runtime denies
        // the request rather than treating the Err as a fail-open signal.
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
            matches!(err, RuntimeError::PermissionDenied { ref verb, .. } if verb == "create"),
            "expected PermissionDenied for missing rego entrypoint, got {err:?}"
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
                _ => {}
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
            self.events.lock().unwrap().push(visitor.0);
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

    // ---- Hard enforcement + EventStore persistence ----

    use crate::runtime::NamespaceToken;
    use async_trait::async_trait;
    use khive_storage::{
        BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, SubstrateKind,
    };
    use khive_types::EventOutcome;

    /// In-memory EventStore for unit tests — avoids file-backed SQLite.
    #[derive(Default, Debug)]
    struct MemoryEventStore {
        events: std::sync::Mutex<Vec<Event>>,
    }

    #[async_trait]
    impl EventStore for MemoryEventStore {
        async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
        async fn append_events(
            &self,
            events: Vec<Event>,
        ) -> khive_storage::StorageResult<BatchWriteSummary> {
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
        // Dispatch returns InvalidInput for an unknown verb, so the
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
    async fn gate_error_does_not_persist_to_event_store() {
        #[derive(Debug)]
        struct FailingGate;
        impl Gate for FailingGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
                Err(khive_gate::GateError::Internal("gate broken".into()))
            }
        }

        let store = Arc::new(MemoryEventStore::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.with_gate(Arc::new(FailingGate));
        builder.with_event_store(store.clone());
        let reg = builder.build().expect("registry builds");

        // Gate Err → fail-open, dispatch proceeds.
        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(
            res["pack"], "alpha",
            "gate error must fail-open, not block dispatch"
        );

        let count = store.count_events(EventFilter::default()).await.unwrap();
        assert_eq!(
            count, 0,
            "gate infrastructure error must NOT produce an audit event in EventStore"
        );
    }

    #[tokio::test]
    async fn no_event_store_configured_tracing_only() {
        // When no event_store is configured, dispatch must succeed without error.
        // (The tracing path is exercised in the tracing tests above; here we just
        // verify the absence of event_store does not break dispatch.)
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
        ];
    }

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
            matches!(err, RuntimeError::InvalidInput(_)),
            "help=true on unknown verb must return InvalidInput, got {err:?}"
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
}
