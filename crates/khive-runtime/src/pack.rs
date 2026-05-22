//! Pack runtime trait and verb registry (ADR-025 step 2).
//!
//! Packs register verbs into the runtime. The registry routes verb calls
//! to the pack that declares them.
//!
//! `Pack` (in khive-types) uses const associated items which are not
//! object-safe. `PackRuntime` mirrors that metadata as methods so the
//! registry can store packs as trait objects. See ADR-025 §PackRuntime.
//!
//! Lifecycle: build with `VerbRegistryBuilder`, then call `.build()` to
//! get a cheaply-cloneable `VerbRegistry`. Registration is only possible
//! through the builder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use khive_gate::{ActorRef, AllowAllGate, AuditEvent, GateDecision, GateRef, GateRequest};
use khive_storage::{Event, EventStore, SubstrateKind};
use khive_types::{EventOutcome, Namespace};
use serde_json::Value;

pub use khive_types::{EdgeEndpointRule, EndpointKind, VerbDef};

/// Hook called after every successful verb dispatch (Issue #158).
///
/// Packs that want to observe real-time dispatch outcomes (e.g. brain pack
/// updating its posteriors) implement this trait and register it via
/// [`VerbRegistryBuilder::with_dispatch_hook`]. The hook is opt-in: when no
/// hook is registered, dispatch incurs zero overhead.
///
/// The hook receives the synthesized `Event` that was built from the dispatch
/// outcome — same representation used by the EventStore audit path — so brain
/// pack's `EventFold` can process it without extra conversion.
#[async_trait]
pub trait DispatchHook: Send + Sync {
    /// Called with the dispatch-outcome event after a successful pack dispatch.
    ///
    /// Errors are logged via `tracing::warn!` and never propagated to the
    /// caller — the dispatch has already succeeded.
    async fn on_dispatch(&self, event: &Event);
}

use crate::error::{
    CircularPackDependency, MissingPackDependencies, MissingPackDependency, RuntimeError,
};
use crate::KhiveRuntime;

/// Async dispatch trait for packs (ADR-025).
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

    /// Verbs this pack handles — must equal `<Self as Pack>::VERBS`.
    fn verbs(&self) -> &'static [VerbDef];

    /// Pack-extensible edge endpoint rules — must equal `<Self as Pack>::EDGE_RULES`.
    /// Defaults to empty so existing packs that don't extend the edge contract
    /// can ignore it (ADR-031).
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        &[]
    }

    /// Pack names whose vocabulary this pack references (ADR-037).
    /// Defaults to empty so existing packs compile without changes.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// Optional per-kind hook for shared CRUD specialization (ADR-030).
    ///
    /// When a kind is owned by this pack (declared in `note_kinds()` or
    /// `entity_kinds()`), returning `Some(hook)` opts that kind into
    /// pack-specific behavior — defaults, derived properties, side-effect
    /// edges — through the shared `create` path. Returning `None` keeps
    /// the kind as plain storage with no specialization.
    fn kind_hook(&self, _kind: &str) -> Option<Arc<dyn KindHook>> {
        None
    }

    /// Dispatch a verb call. Returns serialized JSON response.
    ///
    /// The `registry` parameter gives the handler access to the merged
    /// vocabulary and kind hooks across all loaded packs (ADR-030).
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError>;
}

/// Per-kind specialization for shared CRUD (ADR-030).
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

/// Builder for constructing a `VerbRegistry`.
///
/// Packs are registered here; once `.build()` is called the registry is
/// immutable and cheaply cloneable.
pub struct VerbRegistryBuilder {
    packs: Vec<Box<dyn PackRuntime>>,
    gate: GateRef,
    default_namespace: String,
    /// Optional audit event sink (ADR-035).
    ///
    /// When set, every gate check writes a storage `Event` in addition to the
    /// `tracing::info!` emission. The store is `Arc<dyn EventStore>` so the
    /// registry does not depend on the full `KhiveRuntime` surface — only the
    /// audit-persistence capability is needed here.
    event_store: Option<Arc<dyn EventStore>>,
    /// Optional post-dispatch hook (Issue #158).
    ///
    /// When set, every successful pack dispatch calls `hook.on_dispatch(event)`
    /// with a synthesized Event describing the outcome. Opt-in: when None,
    /// no overhead is incurred.
    dispatch_hook: Option<Arc<dyn DispatchHook>>,
}

impl VerbRegistryBuilder {
    pub fn new() -> Self {
        Self {
            packs: Vec::new(),
            gate: std::sync::Arc::new(AllowAllGate),
            default_namespace: Namespace::default_ns().as_str().to_string(),
            event_store: None,
            dispatch_hook: None,
        }
    }

    /// Register a pack. The bound `P: Pack + PackRuntime` ensures the pack
    /// declares vocabulary via `Pack` consts alongside runtime dispatch.
    pub fn register<P: khive_types::Pack + PackRuntime + 'static>(&mut self, pack: P) -> &mut Self {
        self.packs.push(Box::new(pack));
        self
    }

    /// Register a boxed pack directly (ADR-063).
    ///
    /// Crate-private: only [`PackRegistry::register_packs`] should call this.
    /// External callers must use the typed [`Self::register`] which enforces the
    /// `Pack + PackRuntime` dual-impl contract at the call site.  Here the
    /// contract is satisfied upstream at the [`PackFactory::create`] site.
    pub(crate) fn register_boxed(&mut self, pack: Box<dyn PackRuntime>) -> &mut Self {
        self.packs.push(pack);
        self
    }

    /// Set the authorization gate consulted on every dispatch (ADR-029).
    ///
    /// Defaults to `AllowAllGate` if not set. In v0.2 the gate is **advisory** —
    /// deny decisions are logged via `tracing::warn!` but do not block dispatch.
    pub fn with_gate(&mut self, gate: GateRef) -> &mut Self {
        self.gate = gate;
        self
    }

    /// Set the namespace surfaced to the gate when a verb does not carry an
    /// explicit `namespace` argument. Transports should plumb the runtime's
    /// `default_namespace` so the gate's `input.namespace` always reflects
    /// the operation's true tenant (ADR-029 + ADR-007).
    pub fn with_default_namespace(&mut self, ns: impl Into<String>) -> &mut Self {
        self.default_namespace = ns.into();
        self
    }

    /// Set the `EventStore` used to persist audit events (ADR-035).
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

    /// Register a post-dispatch hook (Issue #158).
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
    /// Performs a topological sort of packs using Kahn's algorithm (ADR-037).
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

        Ok(VerbRegistry {
            packs: Arc::new(ordered_packs),
            gate: self.gate,
            default_namespace: self.default_namespace,
            event_store: self.event_store,
            dispatch_hook: self.dispatch_hook,
        })
    }
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
    gate: GateRef,
    default_namespace: String,
    /// Audit event sink — `None` means tracing-only (v0.2 default) (ADR-035).
    event_store: Option<Arc<dyn EventStore>>,
    /// Post-dispatch hook — `None` means no real-time observation (Issue #158).
    dispatch_hook: Option<Arc<dyn DispatchHook>>,
}

impl VerbRegistry {
    /// Dispatch a verb to the first pack that handles it.
    ///
    /// When multiple packs declare the same verb, the first registered pack wins.
    ///
    /// The configured [`Gate`](khive_gate::Gate) is consulted before dispatch
    /// (ADR-029, ADR-035). `Deny` decisions return
    /// [`RuntimeError::PermissionDenied`] immediately — the pack is never
    /// invoked. `Allow` decisions proceed to pack dispatch as before.
    ///
    /// Every gate consultation emits one `tracing::info!(... "gate.check")` event
    /// with a structured `audit_event` field (ADR-033). When a [`EventStore`]
    /// is configured via [`VerbRegistryBuilder::with_event_store`], an `Event`
    /// is also persisted to the substrate (ADR-035). Storage errors are logged
    /// via `tracing::warn!` and never propagated.
    ///
    /// When `gate.check` itself returns an error (gate infrastructure failure),
    /// the error is logged via `tracing::warn!` and dispatch proceeds (fail-open,
    /// consistent with ADR-029 §Rationale "Why advisory in v0.2"). No audit event
    /// is persisted for an errored gate check — no decision was produced.
    ///
    /// The synthesized `GateRequest` carries `ActorRef::anonymous()` and the
    /// operation's namespace — pulled from `params["namespace"]` when present
    /// (including an explicit empty string, which `KhiveRuntime::ns` also
    /// preserves), otherwise the registry's default namespace (configured via
    /// [`VerbRegistryBuilder::with_default_namespace`]). Gate-visible
    /// namespace and runtime-visible namespace MUST stay aligned; coercing an
    /// empty string here while the runtime keeps `""` would create an
    /// authorization/audit blind spot on the field ADR-029 declares public.
    /// Transports that have richer caller context (auth headers, session
    /// info) will gain a sibling dispatch path in a follow-up.
    pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        // Resolve namespace as an owned String before `params` is moved into
        // pack.dispatch, so the post-dispatch hook can reference it.
        let ns_str: String = params
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.default_namespace.clone());
        let gate_req = GateRequest::new(
            ActorRef::anonymous(),
            Namespace::new(&ns_str),
            verb,
            params.clone(),
        );

        // Consult the gate (ADR-029, ADR-035).
        //
        // - Ok(Allow) → proceed to pack dispatch (tracing + optional EventStore).
        // - Ok(Deny) → emit audit, persist if store configured, return PermissionDenied.
        // - Err(_) → warn via tracing, fail-open (no audit persisted).
        let gate_blocked = match self.gate.check(&gate_req) {
            Ok(decision) => {
                let is_deny = matches!(decision, GateDecision::Deny { .. });

                // Emit audit event via tracing (ADR-033 — preserved path).
                let audit = AuditEvent::from_check(&gate_req, &decision, self.gate.impl_name());
                tracing::info!(
                    audit_event = %serde_json::to_string(&audit)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".into()),
                    "gate.check"
                );

                // Persist to EventStore when configured (ADR-035).
                if let Some(store) = &self.event_store {
                    let outcome = if is_deny {
                        EventOutcome::Denied
                    } else {
                        EventOutcome::Success
                    };
                    let audit_data = serde_json::to_value(&audit).unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "failed to serialize AuditEvent for EventStore");
                        serde_json::Value::Null
                    });
                    let storage_event = Event::new(
                        gate_req.namespace.as_str(),
                        verb,
                        SubstrateKind::Event,
                        format!("{}:{}", gate_req.actor.kind, gate_req.actor.id),
                    )
                    .with_outcome(outcome)
                    .with_data(audit_data);
                    if let Err(store_err) = store.append_event(storage_event).await {
                        tracing::warn!(
                            verb,
                            error = %store_err,
                            "audit event store write failed (non-fatal)"
                        );
                    }
                }

                if is_deny {
                    let reason = match decision {
                        GateDecision::Deny { reason } => reason,
                        _ => String::new(),
                    };
                    Some(reason)
                } else {
                    None
                }
            }
            Err(err) => {
                // Gate infrastructure failure — fail-open (ADR-029 §Rationale).
                // No decision was produced; no audit event is persisted.
                tracing::warn!(verb, error = %err, "gate check failed (fail-open)");
                None
            }
        };

        // Hard enforcement (ADR-035): Deny is now authoritative.
        if let Some(reason) = gate_blocked {
            return Err(RuntimeError::PermissionDenied {
                verb: verb.to_string(),
                reason,
            });
        }

        for pack in self.packs.iter() {
            if pack.verbs().iter().any(|v| v.name == verb) {
                let result = pack.dispatch(verb, params, self).await;

                // Post-dispatch hook: fires on success, opt-in (Issue #158).
                if let (Ok(_), Some(hook)) = (&result, &self.dispatch_hook) {
                    let dispatch_event = Event::new(
                        ns_str.as_str(),
                        verb,
                        SubstrateKind::Event,
                        pack.name(),
                    )
                    .with_outcome(EventOutcome::Success);
                    let hook = Arc::clone(hook);
                    hook.on_dispatch(&dispatch_event).await;
                }

                return result;
            }
        }
        let available: Vec<&str> = self
            .packs
            .iter()
            .flat_map(|p| p.verbs().iter().map(|v| v.name))
            .collect();
        Err(RuntimeError::InvalidInput(format!(
            "unknown verb {verb:?}; available: {}",
            available.join(", ")
        )))
    }

    /// Find a kind hook (ADR-030) among the registered packs.
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

    /// All verb definitions across all registered packs.
    ///
    /// Returned with `'static` lifetime since pack verbs are `&'static [VerbDef]`
    /// constants — callers can keep the slice references beyond the registry's
    /// borrow.
    pub fn all_verbs(&self) -> Vec<&'static VerbDef> {
        self.packs.iter().flat_map(|p| p.verbs().iter()).collect()
    }

    /// All verb definitions paired with the name of the pack that owns them.
    ///
    /// Useful for building catalogs that attribute each verb to its source pack.
    /// The pack name has the same lifetime as `&self`; the `VerbDef` reference
    /// is `'static`.
    pub fn all_verbs_with_names(&self) -> Vec<(&str, &'static VerbDef)> {
        self.packs
            .iter()
            .flat_map(|p| p.verbs().iter().map(move |v| (p.name(), v)))
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

    /// Declared dependencies for a registered pack (ADR-037).
    pub fn pack_requires(&self, name: &str) -> Option<&'static [&'static str]> {
        self.packs
            .iter()
            .find(|p| p.name() == name)
            .map(|p| p.requires())
    }

    /// All pack-declared edge endpoint rules across registered packs (ADR-031).
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
}

// ── ADR-063: inventory-based dynamic pack loading ─────────────────────────────

/// Factory for creating pack instances registered via `inventory` at link time
/// (ADR-063). Each pack crate submits a `&'static dyn PackFactory` wrapped in a
/// [`PackRegistration`]; the binary's linker collects them all into a single
/// slice iterable at runtime.
///
/// Implementors must be `Send + Sync + 'static` because the registry is built
/// once and shared across async tasks.
pub trait PackFactory: Send + Sync + 'static {
    /// Canonical lowercase name for this pack (e.g. `"kg"`, `"gtd"`).
    fn name(&self) -> &'static str;

    /// Names of packs that must be loaded before this one (ADR-037).
    ///
    /// Defaults to empty so pack crates that have no dependencies compile
    /// without changes. [`PackRegistry::register_packs`] uses this to compute
    /// the transitive closure of required packs before registering anything.
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }

    /// Create a new pack instance for the given runtime.
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn PackRuntime>;
}

/// Newtype wrapper collected by `inventory` so pack crates can submit
/// `&'static dyn PackFactory` references without the type-ascription syntax
/// that `inventory::submit!` does not support for bare trait-object references
/// (ADR-063).
pub struct PackRegistration(pub &'static dyn PackFactory);

inventory::collect!(PackRegistration);

/// Registry of pack factories discovered via `inventory` at link time (ADR-063).
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
    /// Resolves transitive `requires()` dependencies declared on each
    /// [`PackFactory`] before registering anything. A pack that declares
    /// `requires = &["kg"]` will cause `"kg"` to be included even if the caller
    /// only asked for `"gtd"`.  The [`VerbRegistryBuilder::build`] topo-sort
    /// then ensures correct load order.
    ///
    /// Returns `Ok(())` when all names (including their transitive deps) are
    /// recognised; returns `Err(name)` for the first unrecognised name so
    /// callers can surface a clear error.
    pub fn register_packs(
        names: &[String],
        runtime: KhiveRuntime,
        builder: &mut VerbRegistryBuilder,
    ) -> Result<(), String> {
        // Build a name→factory index once.
        let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
            .into_iter()
            .map(|r| r.0)
            .collect();
        let factory_for = |name: &str| -> Option<&'static dyn PackFactory> {
            all.iter().copied().find(|f| f.name() == name)
        };

        // BFS transitive closure: start with the explicitly requested names,
        // then walk each factory's requires() to pull in dependencies.
        let mut full_set: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();

        for name in names {
            queue.push_back(name.as_str());
        }

        while let Some(name) = queue.pop_front() {
            if !full_set.insert(name) {
                continue; // already visited
            }
            let factory = factory_for(name).ok_or_else(|| name.to_string())?;
            for &dep in factory.requires() {
                if !full_set.contains(dep) {
                    queue.push_back(dep);
                }
            }
        }

        // Register every pack in the resolved set; VerbRegistryBuilder::build()
        // performs the topo-sort, so insertion order here does not matter.
        for name in &full_set {
            // factory_for cannot fail here: every name in full_set passed the
            // lookup above without returning Err.
            let factory = factory_for(name).unwrap();
            builder.register_boxed(factory.create(runtime.clone()));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::Pack;

    struct AlphaPack;

    impl Pack for AlphaPack {
        const NAME: &'static str = "alpha";
        const NOTE_KINDS: &'static [&'static str] = &["memo", "log"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget"];
        const VERBS: &'static [VerbDef] = &[
            VerbDef {
                name: "create",
                description: "create a widget",
            },
            VerbDef {
                name: "list",
                description: "list widgets",
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
        fn verbs(&self) -> &'static [VerbDef] {
            AlphaPack::VERBS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "alpha", "verb": verb }))
        }
    }

    struct BetaPack;

    impl Pack for BetaPack {
        const NAME: &'static str = "beta";
        const NOTE_KINDS: &'static [&'static str] = &["log", "alert"];
        const ENTITY_KINDS: &'static [&'static str] = &["widget", "gadget"];
        const VERBS: &'static [VerbDef] = &[
            VerbDef {
                name: "notify",
                description: "send alert",
            },
            VerbDef {
                name: "create",
                description: "create a gadget",
            },
        ];
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
        fn verbs(&self) -> &'static [VerbDef] {
            BetaPack::VERBS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
        ) -> Result<Value, RuntimeError> {
            Ok(serde_json::json!({ "pack": "beta", "verb": verb }))
        }
    }

    fn build_registry() -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(AlphaPack);
        builder.register(BetaPack);
        builder.build().expect("registry builds")
    }

    #[tokio::test]
    async fn dispatch_routes_to_correct_pack() {
        let reg = build_registry();

        let res = reg.dispatch("list", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha");

        let res = reg.dispatch("notify", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "beta");
    }

    #[tokio::test]
    async fn dispatch_first_registered_wins_on_collision() {
        let reg = build_registry();

        let res = reg.dispatch("create", Value::Null).await.unwrap();
        assert_eq!(res["pack"], "alpha", "first registered pack wins");
    }

    #[tokio::test]
    async fn dispatch_unknown_verb_returns_error() {
        let reg = build_registry();

        let err = reg.dispatch("explode", Value::Null).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("explode"));
        assert!(msg.contains("create"));
    }

    #[test]
    fn all_verbs_aggregates_across_packs() {
        let reg = build_registry();
        let verbs: Vec<&str> = reg.all_verbs().iter().map(|v| v.name).collect();
        assert_eq!(verbs, vec!["create", "list", "notify", "create"]);
    }

    #[test]
    fn all_verbs_with_names_pairs_pack_name() {
        let reg = build_registry();
        let pairs: Vec<(&str, &str)> = reg
            .all_verbs_with_names()
            .iter()
            .map(|(pack, v)| (*pack, v.name))
            .collect();
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
    fn note_kinds_are_deduplicated() {
        let reg = build_registry();
        let kinds = reg.all_note_kinds();
        assert_eq!(kinds, vec!["memo", "log", "alert"]);
    }

    #[test]
    fn entity_kinds_are_deduplicated() {
        let reg = build_registry();
        let kinds = reg.all_entity_kinds();
        assert_eq!(kinds, vec!["widget", "gadget"]);
    }

    // ---- Gate wiring (ADR-029) ----

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

        // Gate denies — dispatch now returns PermissionDenied (hard enforcement, ADR-035).
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
    // actually receives — codex round-1 caught us hard-wiring `default_ns()`.
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
        // Explicit empty namespace string is preserved (it is what
        // `KhiveRuntime::ns` would also see). Gate and runtime MUST agree on
        // the namespace they observe; coercing here while the runtime
        // continues to honor `""` would create an audit blind spot.
        reg.dispatch("list", serde_json::json!({"namespace": ""}))
            .await
            .unwrap();

        let seen = gate.seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["tenant-y", "tenant-x", ""]);
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

    // ---- Audit event emission (ADR-033) ----

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

        // Gate denies — dispatch returns PermissionDenied (hard enforcement, ADR-035).
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
        // Namespace from params wins (ADR-029 alignment rule).
        assert_eq!(ev.namespace, "tenant-q");
        assert_eq!(ev.verb, "list");
        assert_eq!(ev.actor.kind, "anonymous");
    }

    // ---- Audit tracing emission (ADR-033 §"Emission site") ----
    //
    // The AuditCapturingGate tests above prove that AuditEvent::from_check is
    // called with the right inputs, but they observe the event *inside* the
    // gate impl — they would still pass if dispatch's
    // `tracing::info!(audit_event = ..., "gate.check")` were deleted or
    // renamed. The tests below install a capture Layer and assert on the
    // actual tracing event surfaced from dispatch. This locks the public
    // observability contract from ADR-033: one `gate.check` info event per
    // dispatch, carrying an `audit_event` field that round-trips back to an
    // `AuditEvent`.

    use std::sync::Mutex as StdMutex;

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
    /// another subscriber in the same test binary (Ubuntu CI regression).
    ///
    /// Only captures events emitted on the installing thread.  In CI's
    /// multi-threaded test runner, `#[tokio::test]` futures may emit tracing
    /// events on arbitrary pool threads; filtering by `ThreadId` prevents those
    /// from contaminating the capture buffer of a serial tracing test running
    /// on the same thread at the same wall-clock time.
    struct CaptureSubscriber {
        events: Arc<StdMutex<Vec<CapturedEvent>>>,
        owner_thread: std::thread::ThreadId,
    }

    impl CaptureSubscriber {
        fn new(events: Arc<StdMutex<Vec<CapturedEvent>>>) -> Self {
            Self {
                events,
                owner_thread: std::thread::current().id(),
            }
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
            // Only capture events from the thread that installed this subscriber.
            // Concurrent #[tokio::test] futures on other pool threads must not
            // bleed into this buffer.
            if std::thread::current().id() != self.owner_thread {
                return;
            }
            let mut visitor = CapturedEventVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Run an async block under a scoped tracing subscriber and return the
    /// events captured during the run.
    ///
    /// Uses a minimal `Subscriber` implementation (not `tracing_subscriber::registry()`)
    /// to avoid layer-machinery global-state races on Ubuntu CI. The current-thread
    /// tokio runtime keeps all async work on the same thread as `with_default`, so
    /// the thread-local subscriber captures every `tracing::info!` call in the body.
    ///
    /// Events from other threads are filtered out (see [`CaptureSubscriber`]) so
    /// that concurrent `#[tokio::test]` fixtures cannot inject spurious events.
    fn capture_dispatch_events<Fut>(future: Fut) -> Vec<CapturedEvent>
    where
        Fut: std::future::Future<Output = ()>,
    {
        let captured: Arc<StdMutex<Vec<CapturedEvent>>> = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = CaptureSubscriber::new(Arc::clone(&captured));
        let dispatch = tracing::Dispatch::new(subscriber);

        tracing::dispatcher::with_default(&dispatch, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build current-thread tokio runtime");
            rt.block_on(future);
        });

        let guard = captured.lock().unwrap();
        guard.clone()
    }

    /// Pull every captured event whose `message` matches `"gate.check"`.
    fn gate_check_events(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
        events
            .iter()
            .filter(|e| e.message.as_deref() == Some("gate.check"))
            .collect()
    }

    #[test]
    #[serial]
    fn dispatch_tracing_emits_one_gate_check_event_on_allow() {
        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(AllowAllGate));
            builder.with_default_namespace("tenant-default");
            let reg = builder.build().expect("registry builds");
            reg.dispatch("list", serde_json::json!({"namespace": "tenant-q"}))
                .await
                .unwrap();
        });

        let gate_events = gate_check_events(&events);
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
        assert_eq!(audit.gate_impl, "AllowAllGate");
        assert!(
            audit.deny_reason.is_none(),
            "deny_reason must be None on Allow"
        );
    }

    // ---- Hard enforcement + EventStore persistence (ADR-035) ----

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
            const VERBS: &'static [VerbDef] = &[VerbDef {
                name: "guarded",
                description: "a guarded verb",
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
            fn verbs(&self) -> &'static [VerbDef] {
                Self::VERBS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
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
        struct AlwaysDenyGate;
        impl Gate for AlwaysDenyGate {
            fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
                Ok(GateDecision::deny("denied by test gate"))
            }
            fn impl_name(&self) -> &'static str {
                "AlwaysDenyGate"
            }
        }

        let events = capture_dispatch_events(async {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(AlphaPack);
            builder.with_gate(Arc::new(AlwaysDenyGate));
            let reg = builder.build().expect("registry builds");
            // Hard enforcement (ADR-035) — dispatch returns PermissionDenied on Deny.
            // The tracing audit event is still emitted before the error is returned.
            let _ = reg.dispatch("create", serde_json::Value::Null).await;
        });

        let gate_events = gate_check_events(&events);
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
        assert_eq!(audit.gate_impl, "AlwaysDenyGate");
        // Wire-shape rule from ADR-033: obligations is always serialized as an
        // array, empty on Deny. Round-trip back through serde_json::Value to
        // confirm the field exists on the wire and is `[]`, not missing.
        let payload_json: serde_json::Value =
            serde_json::from_str(payload).expect("payload must be valid JSON");
        assert_eq!(
            payload_json["obligations"],
            serde_json::Value::Array(Vec::new()),
            "obligations must be `[]` on Deny on the tracing payload, not omitted"
        );
    }

    // ---- EventStore audit envelope round-trip (ADR-033 / ADR-035) ----
    //
    // Codex review finding (Major #1): EventStore was persisting a summary
    // Event without the full AuditEvent fields (deny_reason, gate_impl,
    // obligations). This test verifies the complete envelope survives
    // append_event → query_events.

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

        // The data field must hold the full AuditEvent envelope (ADR-033 contract).
        let data = ev
            .data
            .as_ref()
            .expect("Event.data must be Some — full AuditEvent envelope must be persisted");

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.data must deserialize to AuditEvent");

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

        let data = ev
            .data
            .as_ref()
            .expect("Event.data must be Some — AuditEvent envelope must be persisted on allow");

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.data must deserialize to AuditEvent");

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

    // ---- SQL-backed audit envelope round-trip (ADR-033 / ADR-035, codex r2) ----
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
        let sql_store = rt
            .events(Some("test-ns"))
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

        // Event.data must hold the full AuditEvent serialized as JSON text and
        // parsed back. If the SQL path was lossy, this deserialization would fail
        // or the field assertions below would fail.
        let data = ev
            .data
            .as_ref()
            .expect("Event.data must be Some — SqlEventStore must persist AuditEvent envelope");

        let audit: khive_gate::AuditEvent = serde_json::from_value(data.clone())
            .expect("Event.data must deserialize to AuditEvent after SQL round-trip");

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
    // Codex r3 identified a blind spot: the deny-path SQL test above only
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
        let sql_store = rt
            .events(Some("test-ns"))
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

        let data = ev
            .data
            .as_ref()
            .expect("Event.data must be Some — SqlEventStore must persist AuditEvent envelope");

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

    // ---- Audit payload shape for 'create' verb dispatch (ADR-033 / ADR-035) ----
    //
    // The previous audit tests verify the envelope shape for the 'list' verb.
    // This test dispatches 'create' (matching the create_note + annotates path)
    // and verifies that ev.verb, ev.outcome, and ev.data all round-trip correctly
    // through the EventStore. Ensures the ADR-035 wire shape is independent of
    // which verb triggers the gate check.
    #[tokio::test]
    async fn audit_event_payload_shape_for_create_verb_matches_adr035_envelope() {
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

        // Top-level Event fields (ADR-035 §Emission).
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

        // ev.data must hold the full AuditEvent envelope (ADR-033 / ADR-035 contract).
        let data = ev
            .data
            .as_ref()
            .expect("ev.data must be Some — full AuditEvent envelope required by ADR-035");

        let audit: khive_gate::AuditEvent =
            serde_json::from_value(data.clone()).expect("ev.data must deserialize to AuditEvent");

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
            "obligations must be [] on AllowAllGate (wire-shape rule ADR-033)"
        );
    }
}

// ---- ADR-037: inter-pack dependency checking ----

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
        const VERBS: &'static [VerbDef] = &[];
    }

    impl Pack for MemoryDepPack {
        const NAME: &'static str = "memory_dep";
        const NOTE_KINDS: &'static [&'static str] = &["memory"];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const VERBS: &'static [VerbDef] = &[];
        const REQUIRES: &'static [&'static str] = &["kg_dep"];
    }

    impl Pack for ADepPack {
        const NAME: &'static str = "pack_a";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const VERBS: &'static [VerbDef] = &[];
        const REQUIRES: &'static [&'static str] = &["pack_b"];
    }

    impl Pack for BDepPack {
        const NAME: &'static str = "pack_b";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const VERBS: &'static [VerbDef] = &[];
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
        fn verbs(&self) -> &'static [VerbDef] {
            Self::VERBS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
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
        fn verbs(&self) -> &'static [VerbDef] {
            Self::VERBS
        }
        fn requires(&self) -> &'static [&'static str] {
            Self::REQUIRES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
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
        fn verbs(&self) -> &'static [VerbDef] {
            Self::VERBS
        }
        fn requires(&self) -> &'static [&'static str] {
            Self::REQUIRES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
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
        fn verbs(&self) -> &'static [VerbDef] {
            Self::VERBS
        }
        fn requires(&self) -> &'static [&'static str] {
            Self::REQUIRES
        }
        async fn dispatch(
            &self,
            verb: &str,
            _: Value,
            _: &VerbRegistry,
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
            const VERBS: &'static [VerbDef] = &[];
        }

        impl Pack for NoDepsB {
            const NAME: &'static str = "no_deps_b";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const VERBS: &'static [VerbDef] = &[];
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
            fn verbs(&self) -> &'static [VerbDef] {
                Self::VERBS
            }
            async fn dispatch(
                &self,
                verb: &str,
                _: Value,
                _: &VerbRegistry,
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
            fn verbs(&self) -> &'static [VerbDef] {
                Self::VERBS
            }
            async fn dispatch(
                &self,
                verb: &str,
                _: Value,
                _: &VerbRegistry,
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

// ── Dispatch hook tests (Issue #158) ─────────────────────────────────────────

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
        const VERBS: &'static [VerbDef] = &[VerbDef {
            name: "ping",
            description: "ping",
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
        fn verbs(&self) -> &'static [VerbDef] {
            SimplePack::VERBS
        }
        async fn dispatch(
            &self,
            verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
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
        async fn on_dispatch(&self, event: &Event) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_verb.lock().unwrap() = event.verb.clone();
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
            async fn on_dispatch(&self, event: &Event) {
                *self.ns.lock().unwrap() = event.namespace.clone();
            }
        }

        let ns_hook = Arc::new(NsCapturingHook::default());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(SimplePack);
        builder.with_dispatch_hook(ns_hook.clone());
        let reg = builder.build().expect("registry builds");

        reg.dispatch(
            "ping",
            serde_json::json!({"namespace": "tenant-abc"}),
        )
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
