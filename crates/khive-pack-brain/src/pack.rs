//! `BrainPack` struct and inventory factory.

use std::sync::Mutex;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::{HandlerDef, Pack};

use crate::fold::{BalancedRecallFold, SectionPosteriorFold};
use crate::handlers::BRAIN_HANDLERS;
use crate::persist;
use crate::state::BrainState;

pub const ENTITY_CACHE_CAPACITY: usize = 10_000;

/// Sync the `balanced-recall-v1` profile record to match the live `balanced_recall` state.
pub(crate) fn sync_balanced_recall_record(state: &mut BrainState) {
    let total_ev = state.balanced_recall.total_events;
    let snap_val = serde_json::to_value(state.balanced_recall.to_snapshot()).ok();
    if let Some(record) = state.profiles.get_mut("balanced-recall-v1") {
        record.total_events = total_ev;
        record.state_snapshot = snap_val;
    }
}

/// Brain pack — profile-oriented auto-tuning.
pub struct BrainPack {
    pub(crate) runtime: KhiveRuntime,
    /// Profile registry + active balanced-recall state.
    pub(crate) state: Mutex<BrainState>,
    /// Fold for the built-in `balanced-recall-v1` profile.
    pub(crate) fold: BalancedRecallFold,
    /// Fold for per-profile section posteriors.
    pub(crate) section_fold: SectionPosteriorFold,
    /// Tracks which namespaces are loaded from DB and dirty event counts.
    pub(crate) persistence: Mutex<persist::PersistenceTracker>,
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
        let fold = BalancedRecallFold::new(ENTITY_CACHE_CAPACITY);
        let section_fold = SectionPosteriorFold::new();
        let state = BrainState::new(ENTITY_CACHE_CAPACITY);
        Self {
            runtime,
            state: Mutex::new(state),
            fold,
            section_fold,
            persistence: Mutex::new(persist::PersistenceTracker::new()),
        }
    }

    pub(crate) async fn ensure_loaded(&self, token: &NamespaceToken) -> Result<(), RuntimeError> {
        persist::ensure_loaded(
            &self.runtime,
            token,
            &self.persistence,
            &self.state,
            &self.fold,
            &self.section_fold,
            ENTITY_CACHE_CAPACITY,
        )
        .await
    }

    /// Public snapshot of the current `BrainState`.
    pub fn snapshot(&self) -> crate::state::BrainStateSnapshot {
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
