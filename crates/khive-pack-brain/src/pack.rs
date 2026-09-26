//! `BrainPack` struct and inventory factory.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_types::{HandlerDef, Pack};

use khive_brain_core::{BrainSignal, BrainState, ProfileLifecycle, ServeAttribution};

use crate::handlers::BRAIN_HANDLERS;
use crate::persist;

/// Default entity cache capacity for the `balanced-recall-v1` per-entity posterior state.
pub const ENTITY_CACHE_CAPACITY: usize = 10_000;

/// A contended dispatch hook never waits behind a brain handler's SQL work.
/// The handoff is process-local, like the hook's existing in-memory updates.
pub(crate) const MAX_DEFERRED_HOOK_SIGNALS: usize = 1024;

struct HookQueueState {
    pending: VecDeque<(String, BrainSignal)>,
    worker_running: bool,
    dropped: u64,
}

pub(crate) struct HookQueue(Mutex<HookQueueState>);

impl HookQueue {
    fn new() -> Self {
        Self(Mutex::new(HookQueueState {
            pending: VecDeque::new(),
            worker_running: false,
            dropped: 0,
        }))
    }

    /// Keep the most recent bounded handoff signals and elect one drainer.
    fn enqueue(&self, namespace: String, signal: BrainSignal) -> bool {
        let mut queue = self.0.lock().unwrap();
        if queue.pending.len() == MAX_DEFERRED_HOOK_SIGNALS {
            queue.pending.pop_front();
            queue.dropped = queue.dropped.saturating_add(1);
        }
        queue.pending.push_back((namespace, signal));
        if queue.worker_running {
            false
        } else {
            queue.worker_running = true;
            true
        }
    }

    pub(crate) fn worker_running(&self) -> bool {
        self.0.lock().unwrap().worker_running
    }

    /// The empty check and worker reset share the queue lock, so an enqueue
    /// after the reset always elects a new drainer rather than stranding rows.
    fn take_batch_or_stop(&self) -> Option<Vec<(String, BrainSignal)>> {
        let mut queue = self.0.lock().unwrap();
        if queue.pending.is_empty() {
            queue.worker_running = false;
            None
        } else {
            Some(queue.pending.drain(..).collect())
        }
    }

    /// A brain dispatch that owns the gate may claim queued signals before its
    /// handler reads state. Leave the worker elected: it will observe an empty
    /// queue and retire, or process signals that arrived during the handler.
    pub(crate) fn drain_pending(&self) -> Vec<(String, BrainSignal)> {
        self.0.lock().unwrap().pending.drain(..).collect()
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.0.lock().unwrap().dropped
    }
}

#[cfg(test)]
mod hook_queue_tests {
    use super::*;
    use khive_runtime::{Namespace, PackRuntime, VerbRegistryBuilder};
    use serde_json::json;

    #[test]
    fn hook_queue_worker_status_tracks_election_and_drain() {
        let queue = HookQueue::new();
        assert!(!queue.worker_running());
        assert!(queue.enqueue("local".to_string(), BrainSignal::Irrelevant));
        assert!(queue.worker_running());
        assert_eq!(queue.take_batch_or_stop().unwrap().len(), 1);
        assert!(
            queue.worker_running(),
            "worker remains elected until empty check"
        );
        assert!(queue.take_batch_or_stop().is_none());
        assert!(!queue.worker_running());
    }

    #[tokio::test]
    async fn brain_dispatch_applies_queued_cold_signal_before_handler_reads_state() {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let brain = BrainPack::new(runtime.clone());
        // Model a contended hook after worker election but before the worker
        // acquires the dispatch gate. The brain dispatch wins that gate.
        assert!(brain
            .hook_queue
            .enqueue("local".into(), BrainSignal::RecallMiss));

        let registry = VerbRegistryBuilder::new()
            .build()
            .expect("minimal registry");
        let token = runtime.authorize(Namespace::local()).expect("local token");
        brain
            .dispatch("brain.profiles", json!({}), &registry, &token)
            .await
            .expect("promote local namespace");

        assert_eq!(brain.snapshot().balanced_recall.total_events, 1);
        assert!(brain.hook_queue.drain_pending().is_empty());
    }
}

fn apply_hook_signal(
    persistence: &Mutex<persist::PersistenceTracker>,
    state: &Mutex<BrainState>,
    namespace: &str,
    signal: &BrainSignal,
) {
    let target = {
        let mut tracker = persistence.lock().unwrap();
        tracker.route_signal(namespace, signal, ENTITY_CACHE_CAPACITY)
    };
    if matches!(target, persist::ApplyTarget::ActiveSlot) {
        let mut state = state.lock().unwrap();
        crate::apply_dispatch_signal(&mut state, signal);
    }
}

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

#[cfg(test)]
pub(crate) struct FeedbackPrecommitHook {
    pub profile_id: String,
    pub reached_tx: tokio::sync::oneshot::Sender<()>,
    pub proceed_rx: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) static FEEDBACK_PRECOMMIT_HOOK: std::sync::Mutex<Option<FeedbackPrecommitHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_feedback_precommit_hook(hook: FeedbackPrecommitHook) {
    *FEEDBACK_PRECOMMIT_HOOK.lock().unwrap() = Some(hook);
}

#[cfg(test)]
pub(crate) fn clear_feedback_precommit_hook() {
    *FEEDBACK_PRECOMMIT_HOOK.lock().unwrap() = None;
}

#[cfg(test)]
pub(crate) async fn run_feedback_precommit_hook(profile_id: &str) {
    let hook = {
        let mut slot = FEEDBACK_PRECOMMIT_HOOK.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|hook| hook.profile_id == profile_id)
        {
            slot.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.reached_tx.send(());
        let _ = hook.proceed_rx.await;
    }
}

/// Sync the `balanced-recall-v1` profile record's cheap fields to match the
/// live `balanced_recall` state.
///
/// The snapshot is deliberately not written here. It is serialized when it is
/// read, by `BrainState::materialized_profile`, because this function runs on
/// the signal path and the stored value was written far more often than any
/// consumer read it. `total_events` stays eager: it is one integer.
pub(crate) fn sync_balanced_recall_record(state: &mut BrainState) {
    let total_ev = state.balanced_recall.total_events;
    if let Some(record) = state.profiles.get_mut("balanced-recall-v1") {
        record.total_events = total_ev;
    }
}

/// Apply an automatic dispatch-hook signal to the profile that served it.
///
/// Legacy/unspecified signals retain the historical default-profile behavior.
/// Explicitly unattributed signals are dropped, because a failed profile read
/// proves that no profile served. A named profile that is absent from the
/// namespace state also fails closed instead of miscrediting the default.
pub(crate) fn apply_dispatch_signal(state: &mut BrainState, signal: &BrainSignal) {
    let serving_profile = match signal {
        BrainSignal::RecallHit {
            served_by_profile_id,
            serve_attribution,
            ..
        } => match serve_attribution {
            ServeAttribution::Unattributed => return,
            ServeAttribution::Profile => match served_by_profile_id.as_deref() {
                Some(profile_id) => Some(profile_id),
                None => return,
            },
            ServeAttribution::Unspecified => None,
        },
        _ => None,
    };

    let credited_profile = serving_profile.unwrap_or("balanced-recall-v1");
    if state
        .profiles
        .get(credited_profile)
        .is_some_and(|record| record.lifecycle == ProfileLifecycle::Archived)
    {
        return;
    }

    match serving_profile {
        None | Some("balanced-recall-v1") => {
            state.balanced_recall.apply_signal(signal);
            state
                .signals_applied
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            sync_balanced_recall_record(state);
        }
        Some(profile_id) => {
            let Some(profile_state) = state.profile_states.get_mut(profile_id) else {
                tracing::warn!(
                    profile_id,
                    "automatic brain signal named an unavailable serving profile; signal was not applied"
                );
                return;
            };
            profile_state.apply_signal(signal);
            let total_events = profile_state.total_events;
            state
                .signals_applied
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(record) = state.profiles.get_mut(profile_id) {
                record.total_events = total_events;
            }
        }
    }
}

/// Brain pack — profile-management registry.
pub struct BrainPack {
    pub(crate) runtime: KhiveRuntime,
    /// Profile registry + active balanced-recall state.
    pub(crate) state: Arc<Mutex<BrainState>>,
    /// Tracks loaded namespaces, durable snapshot generations, and dirty counts.
    pub(crate) persistence: Arc<Mutex<persist::PersistenceTracker>>,
    /// Serialises the (ensure_loaded → handler) pair so no namespace swap can
    /// occur between the two steps.  Must be a tokio async mutex because the
    /// guard is held across .await points inside dispatch().
    ///
    /// Lock order: dispatch_gate (outermost) → persistence → state.
    /// Nothing inside ensure_loaded or any handler acquires dispatch_gate,
    /// so there is no cycle and no deadlock risk.
    pub(crate) dispatch_gate: Arc<tokio::sync::Mutex<()>>,
    pub(crate) hook_queue: Arc<HookQueue>,
}

impl Pack for BrainPack {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] =
        &[khive_brain_core::ConsumerKind::Recall.as_str()];
    const HANDLERS: &'static [HandlerDef] = BRAIN_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl BrainPack {
    /// Create a new pack bound to the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        let state = BrainState::new(ENTITY_CACHE_CAPACITY);
        Self {
            runtime,
            state: Arc::new(Mutex::new(state)),
            persistence: Arc::new(Mutex::new(persist::PersistenceTracker::new())),
            dispatch_gate: Arc::new(tokio::sync::Mutex::new(())),
            hook_queue: Arc::new(HookQueue::new()),
        }
    }

    /// Called only while holding `dispatch_gate`; the namespace slot cannot
    /// change between the tracker routing decision and state application.
    pub(crate) fn apply_hook_signal(&self, namespace: &str, signal: &BrainSignal) {
        apply_hook_signal(&self.persistence, &self.state, namespace, signal);
    }

    /// A contended hook hands its typed signal to one bounded background
    /// drainer. The worker takes the same gate before routing, preserving the
    /// existing namespace and serving-profile attribution checks.
    pub(crate) fn defer_hook_signal(&self, namespace: String, signal: BrainSignal) {
        if !self.hook_queue.enqueue(namespace, signal) {
            return;
        }
        let queue = Arc::clone(&self.hook_queue);
        let gate = Arc::clone(&self.dispatch_gate);
        let persistence = Arc::clone(&self.persistence);
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            loop {
                let _gate = gate.lock().await;
                let Some(batch) = queue.take_batch_or_stop() else {
                    break;
                };
                for (namespace, signal) in batch {
                    apply_hook_signal(&persistence, &state, &namespace, &signal);
                }
                // Relinquish the gate after each bounded batch so a hot hook
                // stream cannot indefinitely starve brain verb dispatches.
                drop(_gate);
                tokio::task::yield_now().await;
            }
        });
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

    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        self.0.brain_consumer_kinds()
    }

    async fn apply_profile_section_feedback(
        &self,
        token: &NamespaceToken,
        profile_id: &str,
        section_signals: serde_json::Value,
        target_attribution: Option<String>,
    ) -> Result<serde_json::Value, RuntimeError> {
        self.0
            .apply_profile_section_feedback(token, profile_id, section_signals, target_attribution)
            .await
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

    fn register_entity_type_validator_with_types(
        &self,
        runtime: &KhiveRuntime,
        pack_entity_types: &[khive_types::EntityTypeDef],
    ) {
        self.0
            .register_entity_type_validator_with_types(runtime, pack_entity_types)
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
