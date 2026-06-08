//! `BrainPack` struct and inventory factory.

use std::sync::Mutex;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::{HandlerDef, Pack};

use khive_brain_core::BrainState;

use crate::handlers::BRAIN_HANDLERS;
use crate::persist;

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

    #[doc(hidden)]
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
}

inventory::submit! { khive_runtime::PackRegistration(&BrainPackFactory) }
