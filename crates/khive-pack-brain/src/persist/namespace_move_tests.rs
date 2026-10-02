use super::*;
use crate::{BrainPack, ENTITY_CACHE_CAPACITY};
use khive_brain_core::brain_state::{AdapterRecord, RouterStateBlob};
use khive_brain_core::{
    BalancedRecallState, BetaPosterior, EntityPosteriors, ProfileBinding, ProfileRecord,
    SectionPosteriorState, SectionType,
};
use khive_runtime::{Namespace, PackRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uuid::Uuid;

struct MoveHook {
    state_address: usize,
    reached: mpsc::Sender<()>,
    proceed: mpsc::Receiver<()>,
}

static MOVE_HOOK: Mutex<Option<MoveHook>> = Mutex::new(None);

pub(super) fn pause_after_move(state: &Mutex<BrainState>) {
    let hook = {
        let mut slot = MOVE_HOOK.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|hook| hook.state_address == state as *const _ as usize)
        {
            slot.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        hook.reached.send(()).expect("move controller is present");
        hook.proceed
            .recv_timeout(Duration::from_secs(5))
            .expect("move controller releases the loader");
    }
}

struct HookCleanup(usize);

impl Drop for HookCleanup {
    fn drop(&mut self) {
        let mut slot = MOVE_HOOK.lock().unwrap();
        if slot
            .as_ref()
            .is_some_and(|hook| hook.state_address == self.0)
        {
            slot.take();
        }
    }
}

struct Joined<T>(Option<JoinHandle<T>>);

impl<T> Joined<T> {
    fn join(mut self) -> T {
        self.0.take().unwrap().join().expect("observer thread")
    }
}

impl<T> Drop for Joined<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

struct ReleaseProceed(Option<mpsc::Sender<()>>);

impl Drop for ReleaseProceed {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn fixture() -> (Arc<BrainPack>, KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("move-test".into()),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime without model loading");
    let pack = Arc::new(BrainPack::new(runtime.clone()));
    let registry = VerbRegistryBuilder::new().build().expect("empty registry");
    (pack, runtime, registry)
}

fn token(runtime: &KhiveRuntime, name: &str) -> NamespaceToken {
    runtime
        .authorize(Namespace::try_from(name).expect("namespace"))
        .expect("authorized namespace")
}

async fn switch(pack: &BrainPack, registry: &VerbRegistry, token: &NamespaceToken) {
    pack.dispatch("brain.bindings", json!({}), registry, token)
        .await
        .expect("public brain dispatch switches the namespace");
}

fn filled(capacity: usize, count: usize) -> BalancedRecallState {
    let mut state = BalancedRecallState::new(capacity);
    state.relevance = BetaPosterior::new(7.125, 3.25);
    state.salience = BetaPosterior::new(2.375, 8.5);
    state.temporal = BetaPosterior::new(1.625, 9.75);
    state.total_events = 91;
    state.exploration_epoch = 7;
    for index in 1..=count {
        state
            .entity_posteriors
            .get_or_insert(Uuid::from_u128(index as u128), || {
                BetaPosterior::new(index as f64 + 0.125, index as f64 + 0.5)
            });
    }
    if count > 1 {
        // Touch a retained non-newest entry: restore must keep explicit LRU order.
        let id = state.entity_posteriors.order()[0];
        state
            .entity_posteriors
            .get_or_insert(id, BetaPosterior::default);
    }
    state
}

fn seeded(capacity: usize, namespace: &str) -> BrainState {
    let mut state = BrainState::new(capacity);
    state.balanced_recall = filled(capacity, 3);
    for (id, count) in [("extra-alpha", 2), ("extra-beta", 3)] {
        let mut record = ProfileRecord::new_balanced_recall(capacity);
        record.id = id.into();
        record.description = format!("{id} owned description");
        state.profiles.insert(id.into(), record);
        state
            .profile_states
            .insert(id.into(), filled(capacity, count));
    }
    state.bindings.push(ProfileBinding {
        actor: "move-test".into(),
        namespace: namespace.into(),
        consumer_kind: "recall".into(),
        profile_id: "extra-alpha".into(),
        priority: 3,
        created_at: chrono::Utc::now(),
    });
    let mut section = SectionPosteriorState::new();
    section
        .posteriors
        .insert(SectionType::Overview, BetaPosterior::new(5.125, 2.5));
    section.total_events = 13;
    section.exploration_epoch = 19;
    state.section_states.insert("extra-alpha".into(), section);
    state.router_state.insert(
        "extra-alpha".into(),
        RouterStateBlob {
            schema_version: 2,
            gate_bytes: vec![73; 32_768],
        },
    );
    state.adapter_set.insert(
        "extra-alpha".into(),
        vec![AdapterRecord {
            adapter_id: "adapter-owned-name".into(),
            slot: 2,
            content_hash: "owned-checkpoint-hash".into(),
        }],
    );
    state.signals_applied.store(31, Ordering::Relaxed);
    state.snapshot_serializations.store(47, Ordering::Relaxed);
    state
}

// Literal outgoing state reconstruction used before this change.
fn original_park(state: &BrainState, capacity: usize) -> BrainState {
    BrainState {
        profiles: state.profiles.clone(),
        balanced_recall: BalancedRecallState::from_snapshot(
            state.balanced_recall.to_snapshot(),
            capacity,
        ),
        profile_states: state
            .profile_states
            .iter()
            .map(|(id, profile)| {
                (
                    id.clone(),
                    BalancedRecallState::from_snapshot(profile.to_snapshot(), capacity),
                )
            })
            .collect(),
        bindings: state.bindings.clone(),
        section_states: state
            .section_states
            .iter()
            .map(|(id, section)| {
                (
                    id.clone(),
                    SectionPosteriorState::from_snapshot(section.to_snapshot()),
                )
            })
            .collect(),
        router_state: state.router_state.clone(),
        adapter_set: state.adapter_set.clone(),
        signals_applied: std::sync::atomic::AtomicU64::new(
            state.signals_applied.load(Ordering::Relaxed),
        ),
        snapshot_serializations: std::sync::atomic::AtomicU64::new(
            state.snapshot_serializations.load(Ordering::Relaxed),
        ),
    }
}

fn assert_beta_bits(actual: &BetaPosterior, expected: &BetaPosterior) {
    assert_eq!(actual.alpha().to_bits(), expected.alpha().to_bits());
    assert_eq!(actual.beta().to_bits(), expected.beta().to_bits());
}

fn assert_balanced(actual: &BalancedRecallState, expected: &BalancedRecallState) {
    assert_beta_bits(&actual.relevance, &expected.relevance);
    assert_beta_bits(&actual.salience, &expected.salience);
    assert_beta_bits(&actual.temporal, &expected.temporal);
    assert_eq!(
        actual.entity_posteriors.capacity(),
        expected.entity_posteriors.capacity()
    );
    assert_eq!(
        actual.entity_posteriors.order(),
        expected.entity_posteriors.order()
    );
    assert_eq!(actual.total_events, expected.total_events);
    assert_eq!(actual.exploration_epoch, expected.exploration_epoch);
    let actual_snapshot = actual.to_snapshot();
    let expected_snapshot = expected.to_snapshot();
    assert_eq!(
        serde_json::to_value(&actual_snapshot).unwrap(),
        serde_json::to_value(&expected_snapshot).unwrap()
    );
    for (id, posterior) in &expected_snapshot.entity_posteriors {
        assert_beta_bits(actual.entity_posteriors.get(id).unwrap(), posterior);
    }
}

fn assert_brain(actual: &BrainState, expected: &BrainState) {
    assert_balanced(&actual.balanced_recall, &expected.balanced_recall);
    assert_eq!(actual.profile_states.len(), expected.profile_states.len());
    for (id, posterior) in &expected.profile_states {
        assert_balanced(&actual.profile_states[id], posterior);
    }
    for (id, section) in &expected.section_states {
        let actual_section = &actual.section_states[id];
        for (kind, posterior) in &section.posteriors {
            assert_beta_bits(&actual_section.posteriors[kind], posterior);
        }
        for (kind, prior) in &section.priors {
            assert_beta_bits(&actual_section.priors[kind], prior);
        }
    }
    assert_eq!(
        serde_json::to_value(actual.to_snapshot()).unwrap(),
        serde_json::to_value(expected.to_snapshot()).unwrap()
    );
}

fn pointers(state: &BrainState) -> Vec<usize> {
    let profile = &state.profiles["extra-alpha"];
    let key = state.profile_states.get_key_value("extra-alpha").unwrap().0;
    vec![
        profile.id.as_ptr() as usize,
        profile.description.as_ptr() as usize,
        key.as_ptr() as usize,
        state.bindings.as_ptr() as usize,
        state.bindings[0].actor.as_ptr() as usize,
        state.router_state["extra-alpha"].gate_bytes.as_ptr() as usize,
        state.adapter_set["extra-alpha"].as_ptr() as usize,
        state.adapter_set["extra-alpha"][0].adapter_id.as_ptr() as usize,
        state.section_states["extra-alpha"]
            .posteriors
            .get(&SectionType::Overview)
            .unwrap() as *const BetaPosterior as usize,
        state
            .balanced_recall
            .entity_posteriors
            .get(&Uuid::from_u128(1))
            .unwrap() as *const BetaPosterior as usize,
        state.profile_states["extra-alpha"]
            .entity_posteriors
            .get(&Uuid::from_u128(1))
            .unwrap() as *const BetaPosterior as usize,
    ]
}

fn assert_next_evictions(actual: &mut BalancedRecallState, expected: &mut BalancedRecallState) {
    let first_victim = expected.entity_posteriors.order().first().copied();
    let capacity = expected.entity_posteriors.capacity();
    let mut evicted_original = false;
    for index in 0..=capacity {
        let id = Uuid::from_u128(10_000 + index as u128);
        actual
            .entity_posteriors
            .get_or_insert(id, || BetaPosterior::new(4.125, 5.5));
        expected
            .entity_posteriors
            .get_or_insert(id, || BetaPosterior::new(4.125, 5.5));
        assert_balanced(actual, expected);
        if let Some(victim) = first_victim {
            evicted_original |= expected.entity_posteriors.get(&victim).is_none();
        }
    }
    assert!(first_victim.is_none() || evicted_original);
    assert_eq!(actual.entity_posteriors.len(), capacity);
}

#[test]
fn effective_capacity_is_reported_for_zero_one_and_three() {
    for requested in [0, 1, 3] {
        assert_eq!(
            EntityPosteriors::new(requested).capacity(),
            requested.max(1)
        );
    }
}

#[test]
fn equal_capacity_moves_match_literal_restore_and_next_eviction() {
    for requested in [0, 1, 3] {
        let capacity: usize = requested.max(1);
        for count in [0, capacity.saturating_sub(1), capacity, capacity + 4] {
            let mut state = BrainState::new(requested);
            state.balanced_recall = filled(requested, count);
            state
                .profile_states
                .insert("p-one".into(), filled(requested, count));
            state
                .profile_states
                .insert("p-two".into(), filled(requested, count + 1));
            let mut expected = original_park(&state, requested);
            let mut actual = prepare_parked_state(state, requested);
            assert_brain(&actual, &expected);
            assert_next_evictions(&mut actual.balanced_recall, &mut expected.balanced_recall);
            for id in ["p-one", "p-two"] {
                assert_next_evictions(
                    actual.profile_states.get_mut(id).unwrap(),
                    expected.profile_states.get_mut(id).unwrap(),
                );
            }
        }
    }
}

#[test]
fn mixed_capacities_use_literal_restore_independently_even_when_empty() {
    for requested in [0, 1, 3] {
        for count in [0, 1, 7] {
            let mut state = BrainState::new(9);
            state.balanced_recall = filled(9, count);
            state
                .profile_states
                .insert("same".into(), filled(requested, 2));
            state
                .profile_states
                .insert("other".into(), filled(8, count));
            state.signals_applied.store(17, Ordering::Relaxed);
            state.snapshot_serializations.store(29, Ordering::Relaxed);
            let mut expected = original_park(&state, requested);
            let mut actual = prepare_parked_state(state, requested);
            assert_eq!(actual.signals_applied.load(Ordering::Relaxed), 17);
            assert_eq!(actual.snapshot_serializations.load(Ordering::Relaxed), 29);
            assert_brain(&actual, &expected);
            assert_next_evictions(&mut actual.balanced_recall, &mut expected.balanced_recall);
            for id in ["same", "other"] {
                assert_next_evictions(
                    actual.profile_states.get_mut(id).unwrap(),
                    expected.profile_states.get_mut(id).unwrap(),
                );
            }
        }
    }
}

#[tokio::test]
async fn public_switch_moves_owned_allocations_and_keeps_counters() {
    let (pack, runtime, registry) = fixture();
    let a = token(&runtime, "move-a");
    let b = token(&runtime, "move-b");
    switch(&pack, &registry, &a).await;
    let (before_pointers, counters, expected) = {
        let mut state = pack.state.lock().unwrap();
        *state = seeded(ENTITY_CACHE_CAPACITY, "move-a");
        let expected = original_park(&state, ENTITY_CACHE_CAPACITY);
        (pointers(&state), (31, 47), expected)
    };
    switch(&pack, &registry, &b).await;
    {
        let tracker = pack.persistence.lock().unwrap();
        let parked = &tracker.saved_states["move-a"];
        assert_eq!(parked.signals_applied.load(Ordering::Relaxed), counters.0);
        assert_eq!(
            parked.snapshot_serializations.load(Ordering::Relaxed),
            counters.1
        );
        assert_eq!(
            pointers(parked),
            before_pointers,
            "existing owned allocations must move"
        );
        assert_brain(parked, &expected);
    }
    switch(&pack, &registry, &a).await;
    let state = pack.state.lock().unwrap();
    assert_eq!(pointers(&state), before_pointers);
    assert_eq!(state.signals_applied.load(Ordering::Relaxed), 31);
    // assert_brain materialized three profile records in the parked namespace.
    assert_eq!(state.snapshot_serializations.load(Ordering::Relaxed), 50);
    assert_brain(&state, &expected);
}

#[tokio::test]
async fn actual_loader_normalizes_each_outgoing_profile_capacity() {
    let (pack, runtime, _) = fixture();
    let a = token(&runtime, "move-a");
    let b = token(&runtime, "move-b");
    ensure_loaded(&runtime, &a, &pack.persistence, &pack.state, 9)
        .await
        .unwrap();
    let expected = {
        let mut state = pack.state.lock().unwrap();
        *state = seeded(9, "move-a");
        state
            .profile_states
            .insert("extra-alpha".into(), filled(3, 2));
        state
            .profile_states
            .insert("extra-beta".into(), filled(8, 0));
        original_park(&state, 3)
    };
    ensure_loaded(&runtime, &b, &pack.persistence, &pack.state, 3)
        .await
        .unwrap();
    let tracker = pack.persistence.lock().unwrap();
    assert_brain(&tracker.saved_states["move-a"], &expected);
}

#[tokio::test]
async fn same_namespace_new_durable_generation_wins_over_parked_state() {
    let (pack, runtime, registry) = fixture();
    let a = token(&runtime, "move-a");
    switch(&pack, &registry, &a).await;
    *pack.state.lock().unwrap() = seeded(ENTITY_CACHE_CAPACITY, "move-a");
    let mut fresh = seeded(ENTITY_CACHE_CAPACITY, "move-a");
    fresh.bindings[0].priority = 93;
    fresh.balanced_recall.total_events = 777;
    let snapshot = fresh.to_snapshot();
    upsert_snapshot(runtime.sql().as_ref(), "move-a", &snapshot, 500_000)
        .await
        .unwrap();
    switch(&pack, &registry, &a).await;
    let state = pack.state.lock().unwrap();
    let expected = BrainState::from_snapshot(snapshot, ENTITY_CACHE_CAPACITY);
    assert_eq!(state.bindings[0].priority, 93);
    assert_eq!(state.balanced_recall.total_events, 777);
    assert_eq!(state.signals_applied.load(Ordering::Relaxed), 0);
    assert_brain(&state, &expected);
    drop(state);
    assert!(!pack
        .persistence
        .lock()
        .unwrap()
        .saved_states
        .contains_key("move-a"));
}

#[tokio::test]
async fn cold_incoming_state_replays_events_then_drains_queued_hooks() {
    let (pack, runtime, registry) = fixture();
    let a = token(&runtime, "move-a");
    let b = token(&runtime, "move-b");
    switch(&pack, &registry, &a).await;
    let mut fresh = BrainState::new(ENTITY_CACHE_CAPACITY);
    fresh.balanced_recall.total_events = 9;
    let snapshot = fresh.to_snapshot();
    upsert_snapshot(runtime.sql().as_ref(), "move-b", &snapshot, 500_000)
        .await
        .unwrap();
    let mut event = Event::new(
        "move-b",
        "memory.recall",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Note,
        "move-test",
    );
    event.created_at = 600_000;
    append_brain_event(
        runtime.sql().as_ref(),
        "move-b",
        "balanced-recall-v1",
        "memory.recall",
        &serde_json::to_value(event).unwrap(),
        600_000,
    )
    .await
    .unwrap();
    let expected = {
        let mut state = BrainState::from_snapshot(snapshot, ENTITY_CACHE_CAPACITY);
        state.balanced_recall.apply_signal(&BrainSignal::RecallMiss);
        crate::ensure_section_state_seeded(&mut state.section_states, "balanced-recall-v1")
            .apply_signal(&BrainSignal::RecallMiss);
        crate::sync_balanced_recall_record(&mut state);
        crate::apply_dispatch_signal(&mut state, &BrainSignal::RecallMiss);
        state
    };
    pack.persistence.lock().unwrap().route_signal(
        "move-b",
        &BrainSignal::RecallMiss,
        ENTITY_CACHE_CAPACITY,
    );
    switch(&pack, &registry, &b).await;
    let state = pack.state.lock().unwrap();
    assert_eq!(state.balanced_recall.total_events, 11);
    assert_eq!(state.signals_applied.load(Ordering::Relaxed), 1);
    assert_brain(&state, &expected);
}

#[tokio::test]
async fn public_snapshot_blocks_until_incoming_state_is_published() {
    let (pack, runtime, registry) = fixture();
    let a = token(&runtime, "move-a");
    let b = token(&runtime, "move-b");
    switch(&pack, &registry, &b).await;
    *pack.state.lock().unwrap() = seeded(ENTITY_CACHE_CAPACITY, "move-b");
    switch(&pack, &registry, &a).await;
    let expected = {
        let tracker = pack.persistence.lock().unwrap();
        serde_json::to_value(tracker.saved_states["move-b"].to_snapshot()).unwrap()
    };
    let address = Arc::as_ptr(&pack.state) as usize;
    let (reached_tx, reached_rx) = mpsc::channel();
    let (proceed_tx, proceed_rx) = mpsc::channel();
    *MOVE_HOOK.lock().unwrap() = Some(MoveHook {
        state_address: address,
        reached: reached_tx,
        proceed: proceed_rx,
    });
    let _cleanup = HookCleanup(address);
    let observed_pack = Arc::clone(&pack);
    let controller = Joined(Some(thread::spawn(move || {
        let release = ReleaseProceed(Some(proceed_tx));
        reached_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("loader reached real move");
        let state_is_locked = observed_pack.state.try_lock().is_err();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let snapshot_pack = Arc::clone(&observed_pack);
        let observer = Joined(Some(thread::spawn(move || {
            started_tx.send(()).unwrap();
            let value = serde_json::to_value(snapshot_pack.snapshot()).unwrap();
            result_tx.send(value).unwrap();
        })));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let early = result_rx.recv_timeout(Duration::from_millis(100)).ok();
        let returned_before_release = early.is_some();
        drop(release);
        let snapshot =
            early.unwrap_or_else(|| result_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        observer.join();
        (state_is_locked, returned_before_release, snapshot)
    })));
    switch(&pack, &registry, &b).await;
    let (state_is_locked, returned_before_release, snapshot) = controller.join();
    assert!(
        state_is_locked,
        "state guard must span the real move and publication"
    );
    assert!(
        !returned_before_release,
        "public snapshot must block while placeholder is held"
    );
    assert_eq!(
        snapshot, expected,
        "public snapshot returns complete incoming state"
    );
}
