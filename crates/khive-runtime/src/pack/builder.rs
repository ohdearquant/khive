use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

#[cfg(doc)]
use khive_gate::GateRequest;
use khive_gate::{AllowAllGate, GateRef};
use khive_storage::EventStore;
#[cfg(doc)]
use khive_storage::EventView;
use khive_types::Namespace;
use serde_json::Value;

use crate::error::{
    CircularPackDependency, MissingPackDependencies, MissingPackDependency, RuntimeError,
};
use crate::KhiveRuntime;

use super::{
    DispatchHook, EdgeEndpointRule, EntityTypeDef, HandlerDef, PackByIdResolver, PackRuntime,
    VerbCategory, VerbRegistry, Visibility, RESERVED_ENVELOPE_ARGS,
};
#[cfg(doc)]
use super::{PackFactory, PackRegistry};

/// Builder for constructing a `VerbRegistry`.
///
/// Packs are registered here; once `.build()` is called the registry is
/// immutable and cheaply cloneable.
pub struct VerbRegistryBuilder {
    packs: Vec<Box<dyn PackRuntime>>,
    pub(super) pack_versions: HashMap<String, &'static str>,
    /// Parallel to `packs`: whether the composition root vouches for the
    /// pack at the same index, recorded by the registration method the
    /// *caller* chose rather than anything the pack reports about itself.
    /// [`Self::register`] (public, reachable from any pack crate) always
    /// pushes `false`; `register_boxed` (crate-private, exercised only by
    /// [`PackRegistry::register_packs`]'s `inventory`-discovered factories)
    /// and the test-only `register_trusted` push `true`. A pack has no API
    /// surface to set its own entry here — see
    /// [`VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS`]'s doc for why
    /// `pack.name()` alone cannot be trusted for this decision.
    pack_trusted: Vec<bool>,
    resolvers: Vec<(String, Box<dyn PackByIdResolver>)>,
    pub(super) kg_read_resolver: Option<Arc<crate::kg_read::KgReadResolver>>,
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
    /// Defers the runtime sink's namespace-scoped read binding until build.
    runtime_event_store: Option<KhiveRuntime>,
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
            pack_versions: HashMap::new(),
            pack_trusted: Vec::new(),
            resolvers: Vec::new(),
            kg_read_resolver: None,
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::local().as_str().to_string(),
            visible_namespaces: vec![],
            actor_id: None,
            event_store: None,
            runtime_event_store: None,
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
    ///
    /// This is the untrusted path: reachable from any external pack crate,
    /// so the pack registered here is never eligible for admission-degrade
    /// under `VerbRegistry::admission_degrade_safe`, regardless of what
    /// `pack.name()`/handler category it reports. Use `register_boxed`
    /// (composition root) or `register_trusted` (tests) for a pack the
    /// caller actually vouches for.
    pub fn register<P: khive_types::Pack + PackRuntime + 'static>(&mut self, pack: P) -> &mut Self {
        self.packs.push(Box::new(pack));
        self.pack_trusted.push(false);
        self
    }

    /// Register a boxed pack directly, vouched for by the composition root.
    ///
    /// Crate-private: only [`PackRegistry::register_packs`]/
    /// `register_packs_with_runtimes` should call this — both resolve the
    /// pack from an `inventory`-discovered `&'static dyn PackFactory`
    /// (collected at link time from `inventory::submit!` call sites, not
    /// from request-time data), so the trust grant recorded here reflects a
    /// decision the composition root made, never something the pack itself
    /// supplied. External callers must use the typed [`Self::register`]
    /// which enforces the `Pack + PackRuntime` dual-impl contract at the
    /// call site but is never trusted. Here the `Pack + PackRuntime`
    /// contract is satisfied upstream at the [`PackFactory::create`] site.
    pub(crate) fn register_boxed(&mut self, pack: Box<dyn PackRuntime>) -> &mut Self {
        self.packs.push(pack);
        self.pack_trusted.push(true);
        self
    }

    /// Register an owned mounted namespace without native-pack trust privileges.
    pub fn register_mounted(
        &mut self,
        pack: Box<dyn PackRuntime>,
    ) -> Result<&mut Self, RuntimeError> {
        if pack.mounted_namespace() != Some(pack.name()) || !pack.handlers().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "invalid mounted namespace registration".into(),
            ));
        }
        self.packs.push(pack);
        self.pack_trusted.push(false);
        Ok(self)
    }

    /// Test-only trusted registration, mirroring `register_boxed`'s trust
    /// grant for external test binaries (e.g.
    /// `tests/read_verb_admission_exhaustion.rs`) that cannot reach a
    /// crate-private method directly — the same reason
    /// [`VerbRegistry::admission_degrade_safe_probe`] is `pub` rather than
    /// `pub(crate)`. A test using this method is asserting that the pack it
    /// registers stands in for a pack the real composition root would load,
    /// not an untrusted/third-party one.
    #[cfg(any(test, feature = "test-internals"))]
    pub fn register_trusted<P: khive_types::Pack + PackRuntime + 'static>(
        &mut self,
        pack: P,
    ) -> &mut Self {
        self.packs.push(Box::new(pack));
        self.pack_trusted.push(true);
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
        self.runtime_event_store = None;
        self.audit_store_read_only = false;
        self
    }

    /// Configure the registry's trusted audit sink from a runtime.
    ///
    /// Registry audit constructors stamp namespace and actor directly from
    /// each resolved [`GateRequest`], including per-request daemon identity
    /// overrides. This deliberately uses the runtime's undecorated sink: the
    /// public token-scoped [`KhiveRuntime::events`] decorator would otherwise
    /// replace every per-request stamp with the single actor that happened to
    /// construct the registry.
    ///
    /// The sink is resolved during [`Self::build`] using the final default
    /// namespace, so the order of namespace and sink configuration does not
    /// change its read scope. Sink initialization errors are returned by build:
    /// a serving registry never silently drops a configured runtime audit sink.
    /// Metadata builds and explicit replacement sinks do not open this sink.
    pub fn with_runtime_event_store(
        &mut self,
        runtime: &KhiveRuntime,
    ) -> Result<&mut Self, RuntimeError> {
        self.event_store = None;
        self.runtime_event_store = Some(runtime.clone());
        self.audit_store_read_only = false;
        Ok(self)
    }

    /// Override the ADR-133 audit-batch seam's tunables, applied when
    /// `build()` lazily constructs the batch from `event_store`.
    /// `None` (the default) uses `AuditBatchConfig::default()`. Exposed for
    /// tests that need to force a small `max_pending_rows` or a short
    /// `admission_deadline` to exercise admission-pressure paths
    /// deterministically (#2117, #2147, #2208, #2217).
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
        self.runtime_event_store = None;
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
        self.build_registry(true)
    }

    /// Inspect pack metadata without activating any registered pack.
    /// The result exposes no dispatch, preparation hooks, or serving-registry conversion.
    pub fn build_metadata(mut self) -> Result<PackMetadataRegistry, RuntimeError> {
        self.event_store = None;
        self.runtime_event_store = None;
        self.dispatch_hook = None;
        self.resolvers.clear();
        self.build_registry(false)
            .map(|registry| PackMetadataRegistry { registry })
    }

    fn build_registry(self, activate: bool) -> Result<VerbRegistry, RuntimeError> {
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

        for mounted in packs
            .iter()
            .filter(|pack| pack.mounted_namespace().is_some())
        {
            let prefix = format!("{}.", mounted.name());
            if packs
                .iter()
                .flat_map(|pack| pack.handlers())
                .any(|handler| handler.name.starts_with(&prefix))
            {
                return Err(RuntimeError::InvalidInput(
                    "mounted namespace collides with a native verb".into(),
                ));
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

        let mut pack_slots: Vec<Option<Box<dyn PackRuntime>>> =
            packs.into_iter().map(Some).collect();
        let mut trusted_slots: Vec<Option<bool>> =
            self.pack_trusted.into_iter().map(Some).collect();
        let mut ordered_packs: Vec<Box<dyn PackRuntime>> = Vec::with_capacity(pack_slots.len());
        let mut ordered_trusted: Vec<bool> = Vec::with_capacity(trusted_slots.len());
        for idx in ordered_indices {
            ordered_packs.push(
                pack_slots[idx]
                    .take()
                    .expect("topological index must exist"),
            );
            ordered_trusted.push(
                trusted_slots[idx]
                    .take()
                    .expect("topological index must exist"),
            );
        }

        validate_unique_note_kinds(&ordered_packs)?;
        validate_unique_verb_names(&ordered_packs)?;
        validate_unique_entity_types(&ordered_packs)?;
        validate_entity_type_note_kind_collisions(&ordered_packs)?;
        validate_brain_consumer_kinds(&ordered_packs)?;
        if activate {
            for pack in &ordered_packs {
                pack.validate_config()?;
            }
        }

        let available_verbs: Vec<&'static str> = ordered_packs
            .iter()
            .flat_map(|p| p.handlers().iter())
            .filter(|h| matches!(h.visibility, Visibility::Verb))
            .map(|h| h.name)
            .collect();

        // Keep the first declaration for duplicate subhandler names, matching
        // the registry's existing pack-order metadata lookup behavior. Public
        // verb names have already been checked for uniqueness above.
        let mut handler_by_name: HashMap<&'static str, &'static HandlerDef> = HashMap::new();
        for pack in &ordered_packs {
            for handler in pack.handlers() {
                handler_by_name.entry(handler.name).or_insert(handler);
            }
        }

        // Admission-degrade eligibility (#2147/#2217, khive-oss#2311): decided
        // once here, from the trust bit the composition root recorded at
        // registration time (never from `pack.name()`'s self-report) plus
        // each handler's declared category and the `(pack, verb)` allowlist —
        // see `VerbRegistry::admission_degrade_safe`'s doc. A verb is
        // globally unique across `Visibility::Verb` handlers at this point
        // (`validate_unique_verb_names` above already enforced that), so a
        // flat `HashSet<&'static str>` is an unambiguous key: no dispatch
        // call site needs to re-resolve which pack owns a verb to answer
        // this question, and none does (`VerbRegistry::admission_degrade_safe`
        // is a single hash-set lookup with no per-call pack/handler scan).
        let mut degrade_safe_verbs: HashSet<&'static str> = HashSet::new();
        let mut read_replay_safe_verbs = HashSet::new();
        for (pack, &trusted) in ordered_packs.iter().zip(ordered_trusted.iter()) {
            if !trusted {
                continue;
            }
            let pack_name = pack.name();
            for handler in pack.handlers() {
                let canonical_owner = handler
                    .name
                    .split_once('.')
                    .map_or("kg", |(owner, _)| owner);
                if matches!(handler.visibility, Visibility::Verb)
                    && pack_name == canonical_owner
                    && crate::classify_operation(handler.name) == Some(crate::OperationAccess::Read)
                    && !VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS.contains(&handler.name)
                {
                    read_replay_safe_verbs.insert(handler.name);
                }
                if !matches!(handler.visibility, Visibility::Verb)
                    || handler.category != VerbCategory::Assertive
                {
                    continue;
                }
                let eligible = VerbRegistry::admission_degrade_safe_sorted()
                    .binary_search_by(|&(p, v)| p.cmp(pack_name).then_with(|| v.cmp(handler.name)))
                    .is_ok();
                if eligible {
                    degrade_safe_verbs.insert(handler.name);
                }
            }
        }

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
        let event_store = match self.runtime_event_store {
            Some(runtime) => Some(runtime.raw_events_for_namespace(&self.default_namespace)?),
            None => self.event_store,
        };
        if let Some(store) = &event_store {
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
        let audit_batch = event_store.clone().map(|store| {
            crate::audit_batch::AuditBatch::new(
                store,
                self.audit_batch_config.clone().unwrap_or_default(),
            )
        });

        Ok(VerbRegistry {
            packs: Arc::new(ordered_packs),
            pack_versions: Arc::new(self.pack_versions),
            resolvers: Arc::new(self.resolvers),
            kg_read_resolver: self.kg_read_resolver,
            gate: self.gate,
            default_namespace: self.default_namespace,
            visible_namespaces: self.visible_namespaces,
            actor_id: self.actor_id,
            event_store,
            audit_store_read_only: self.audit_store_read_only,
            dispatch_hook: self.dispatch_hook,
            available_verbs: Arc::new(available_verbs),
            handler_by_name: Arc::new(handler_by_name),
            degrade_safe_verbs: Arc::new(degrade_safe_verbs),
            read_replay_safe_verbs: Arc::new(read_replay_safe_verbs),
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

/// A granular kind token must identify only one substrate after pack composition.
/// Check aliases too: both subtype and note-kind spellings are normalized at
/// the request boundary, so a cosmetic spelling difference is still a clash.
fn validate_entity_type_note_kind_collisions(
    packs: &[Box<dyn PackRuntime>],
) -> Result<(), RuntimeError> {
    let mut note_kinds = HashMap::new();
    for pack in packs {
        for &kind in pack.note_kinds() {
            note_kinds
                .entry(khive_types::to_snake_case(kind))
                .or_insert(pack.name());
        }
    }

    let check_definition = |definition: &EntityTypeDef, owner: &str| {
        for name in std::iter::once(definition.type_name).chain(definition.aliases.iter().copied())
        {
            let normalized = khive_types::to_snake_case(name);
            if let Some(note_owner) = note_kinds.get(&normalized) {
                return Err(RuntimeError::InvalidInput(format!(
                    "entity subtype {name:?} from {owner:?} collides with note kind {normalized:?} from pack {note_owner:?}"
                )));
            }
        }
        Ok(())
    };

    let builtin = khive_types::EntityTypeRegistry::builtin();
    for definition in builtin.definitions() {
        check_definition(definition, "builtin")?;
    }
    for pack in packs {
        for definition in pack.entity_types() {
            check_definition(definition, pack.name())?;
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

/// Pack metadata with no executable registry capability.
///
/// ```compile_fail
/// fn dispatch(metadata: &khive_runtime::PackMetadataRegistry) {
///     metadata.dispatch("telemetry.emit", serde_json::json!({}));
/// }
/// ```
pub struct PackMetadataRegistry {
    pub(super) registry: VerbRegistry,
}

impl PackMetadataRegistry {
    pub fn has_verb(&self, verb: &str) -> bool {
        self.registry.has_verb(verb)
    }

    pub fn describe_verb(&self, verb: &str) -> Result<Value, RuntimeError> {
        self.registry.describe_verb(verb)
    }

    pub fn all_handlers_with_names(&self) -> Vec<(&str, &'static HandlerDef)> {
        self.registry.all_handlers_with_names()
    }

    pub fn all_verbs(&self) -> Vec<&'static HandlerDef> {
        self.registry.all_verbs()
    }

    pub fn pack_names(&self) -> Vec<&str> {
        self.registry.pack_names()
    }

    /// Factory-reported version, if this pack was registered through a factory.
    pub fn pack_version(&self, name: &str) -> Option<&'static str> {
        self.registry.pack_version(name)
    }

    pub fn pack_requires(&self, name: &str) -> Option<&'static [&'static str]> {
        self.registry.pack_requires(name)
    }

    pub fn pack_note_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.registry.pack_note_kinds(name)
    }

    pub fn pack_entity_kinds(&self, name: &str) -> Option<&'static [&'static str]> {
        self.registry.pack_entity_kinds(name)
    }

    pub fn pack_verbs(&self, name: &str) -> Option<&'static [HandlerDef]> {
        self.registry.pack_verbs(name)
    }

    pub fn all_entity_kinds(&self) -> Vec<&'static str> {
        self.registry.all_entity_kinds()
    }

    pub fn all_note_kinds(&self) -> Vec<&'static str> {
        self.registry.all_note_kinds()
    }

    pub fn all_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.registry.all_edge_rules()
    }
}
