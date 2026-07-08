//! `BrainPack` struct and inventory factory.

use std::sync::Mutex;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::{HandlerDef, Pack};

use khive_brain_core::BrainState;

use crate::handlers::BRAIN_HANDLERS;
use crate::persist;

/// Default entity cache capacity for the `balanced-recall-v1` per-entity posterior state.
pub const ENTITY_CACHE_CAPACITY: usize = 10_000;

// Test-only hook that fires inside dispatch(), after ensure_loaded returns and
// before the handler acquires self.state.  Lets tests inject a concurrent
// namespace swap to prove the dispatch gate prevents cross-namespace pollution.
#[cfg(test)]
pub(crate) struct DispatchHook {
    pub reached_tx: tokio::sync::oneshot::Sender<()>,
    pub proceed_rx: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) static DISPATCH_INTERLEAVE_HOOK: std::sync::Mutex<Option<DispatchHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_dispatch_interleave_hook(hook: DispatchHook) {
    *DISPATCH_INTERLEAVE_HOOK.lock().unwrap() = Some(hook);
}

#[cfg(test)]
pub(crate) fn clear_dispatch_interleave_hook() {
    *DISPATCH_INTERLEAVE_HOOK.lock().unwrap() = None;
}

/// Sync the `balanced-recall-v1` profile record to match the live `balanced_recall` state.
pub(crate) fn sync_balanced_recall_record(state: &mut BrainState) {
    let total_ev = state.balanced_recall.total_events;
    let snap_val = serde_json::to_value(state.balanced_recall.to_snapshot()).ok();
    if let Some(record) = state.profiles.get_mut("balanced-recall-v1") {
        record.total_events = total_ev;
        record.state_snapshot = snap_val;
    }
}

/// Brain pack — profile-management registry.
pub struct BrainPack {
    pub(crate) runtime: KhiveRuntime,
    /// Profile registry + active balanced-recall state.
    pub(crate) state: Mutex<BrainState>,
    /// Tracks which namespaces are loaded from DB and dirty event counts.
    pub(crate) persistence: Mutex<persist::PersistenceTracker>,
    /// Serialises the (ensure_loaded → handler) pair so no namespace swap can
    /// occur between the two steps.  Must be a tokio async mutex because the
    /// guard is held across .await points inside dispatch().
    ///
    /// Lock order: dispatch_gate (outermost) → persistence → state.
    /// Nothing inside ensure_loaded or any handler acquires dispatch_gate,
    /// so there is no cycle and no deadlock risk.
    pub(crate) dispatch_gate: tokio::sync::Mutex<()>,
}

impl Pack for BrainPack {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = BRAIN_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl BrainPack {
    /// Create a new pack bound to the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let state = BrainState::new(ENTITY_CACHE_CAPACITY);
        Self {
            runtime,
            state: Mutex::new(state),
            persistence: Mutex::new(persist::PersistenceTracker::new()),
            dispatch_gate: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub fn activate_namespace_for_test(&self, namespace: &str) {
        self.persistence
            .lock()
            .unwrap()
            .mark_loaded(namespace.into());
    }

    pub(crate) async fn ensure_loaded(&self, token: &NamespaceToken) -> Result<(), RuntimeError> {
        persist::ensure_loaded(
            &self.runtime,
            token,
            &self.persistence,
            &self.state,
            ENTITY_CACHE_CAPACITY,
        )
        .await
    }

    /// Public snapshot of the current `BrainState`.
    pub fn snapshot(&self) -> khive_brain_core::BrainStateSnapshot {
        self.state.lock().unwrap().to_snapshot()
    }

    /// Return the `total_events` counter for a namespace stored in the cold/saved
    /// state buckets inside `PersistenceTracker`.  Returns `None` when no state
    /// has been initialised for the given namespace.
    ///
    /// Intended for test verification only.  Production code should access state
    /// via `ensure_loaded` + `snapshot()`.
    #[cfg(test)]
    pub fn cold_namespace_total_events(&self, namespace: &str) -> Option<u64> {
        self.persistence.lock().unwrap().total_events_for(namespace)
    }
}

struct BrainPackFactory;

impl khive_runtime::PackFactory for BrainPackFactory {
    fn name(&self) -> &'static str {
        "brain"
    }

    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }

    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::pack::PackRuntime> {
        Box::new(BrainPack::new(runtime))
    }

    // Overrides the default `create`-based install so the dispatch hook
    // observes the exact same `BrainPack` instance the runtime mutates,
    // instead of a second, state-divergent instance.
    fn create_install(&self, runtime: KhiveRuntime) -> khive_runtime::PackInstall {
        let brain = std::sync::Arc::new(BrainPack::new(runtime));
        khive_runtime::PackInstall {
            runtime: Box::new(BrainPackRuntime(std::sync::Arc::clone(&brain))),
            resolver: None,
            dispatch_hook: Some(brain),
        }
    }
}

/// Forwards the full `PackRuntime` surface to the shared inner `BrainPack`
/// instance so the pack registry's runtime and the registered dispatch hook
/// (see `create_install`) observe the same state and persistence tracker.
struct BrainPackRuntime(std::sync::Arc<BrainPack>);

#[async_trait::async_trait]
impl khive_runtime::pack::PackRuntime for BrainPackRuntime {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        self.0.note_kinds()
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        self.0.entity_kinds()
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        self.0.handlers()
    }

    fn edge_rules(&self) -> &'static [khive_types::EdgeEndpointRule] {
        self.0.edge_rules()
    }

    fn requires(&self) -> &'static [&'static str] {
        self.0.requires()
    }

    fn note_kind_specs(&self) -> &'static [khive_runtime::NoteKindSpec] {
        self.0.note_kind_specs()
    }

    fn kind_hook(&self, kind: &str) -> Option<std::sync::Arc<dyn khive_runtime::KindHook>> {
        self.0.kind_hook(kind)
    }

    fn schema_plan(&self) -> khive_runtime::SchemaPlan {
        self.0.schema_plan()
    }

    fn validation_rules(&self) -> &'static [khive_runtime::ValidationRule] {
        self.0.validation_rules()
    }

    fn register_embedders(&self, runtime: &KhiveRuntime) {
        self.0.register_embedders(runtime)
    }

    fn register_entity_type_validator(&self, runtime: &KhiveRuntime) {
        self.0.register_entity_type_validator(runtime)
    }

    async fn warm(&self) {
        self.0.warm().await
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: serde_json::Value,
        registry: &khive_runtime::VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        self.0.dispatch(verb, params, registry, token).await
    }
}

inventory::submit! { khive_runtime::PackRegistration(&BrainPackFactory) }
