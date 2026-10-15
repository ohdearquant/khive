use khive_brain_core::{
    BalancedRecallState, BrainSignal, BrainState, ProfileLifecycle, ServeAttribution,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use uuid::Uuid;

fn hit(profile: Option<&str>, attribution: ServeAttribution) -> BrainSignal {
    BrainSignal::RecallHit {
        target_id: Uuid::new_v4(),
        latency_us: 1_000,
        served_by_profile_id: profile.map(str::to_string),
        serve_attribution: attribution,
    }
}

fn counts(state: &BrainState) -> (u64, u64) {
    (
        state.signals_applied.load(Ordering::Relaxed),
        state.snapshot_serializations.load(Ordering::Relaxed),
    )
}

#[test]
fn the_default_profile_path_counts_every_signal_and_serializes_none_of_them() {
    let mut state = BrainState::new(8);
    for _ in 0..5 {
        crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unspecified));
    }
    assert_eq!(
        counts(&state),
        (5, 0),
        "five signals, no reader: the snapshot has no reason to exist yet"
    );
    assert_eq!(
        state.profiles["balanced-recall-v1"].total_events, 5,
        "total_events stays eager: it is one integer"
    );
}

#[test]
fn a_named_resident_profile_counts_on_its_own_branch() {
    let mut state = BrainState::new(8);
    state
        .profile_states
        .insert("other-v1".to_string(), BalancedRecallState::new(8));
    for _ in 0..3 {
        crate::apply_dispatch_signal(
            &mut state,
            &hit(Some("other-v1"), ServeAttribution::Profile),
        );
    }
    assert_eq!(counts(&state), (3, 0));
}

#[test]
fn an_unattributed_signal_is_dropped_and_counts_nothing() {
    let mut state = BrainState::new(8);
    crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unattributed));
    assert_eq!(
        counts(&state),
        (0, 0),
        "a failed profile read proves no profile served, so nothing was applied to count"
    );
}

#[test]
fn archived_default_and_named_profiles_ignore_dispatch_signals() {
    let mut state = BrainState::new(8);
    let mut named = state.profiles["balanced-recall-v1"].clone();
    named.id = "archived-v1".into();
    named.lifecycle = ProfileLifecycle::Archived;
    state.profiles.insert(named.id.clone(), named);
    state
        .profile_states
        .insert("archived-v1".into(), BalancedRecallState::new(8));
    state
        .profiles
        .get_mut("balanced-recall-v1")
        .unwrap()
        .lifecycle = ProfileLifecycle::Archived;

    crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unspecified));
    crate::apply_dispatch_signal(
        &mut state,
        &hit(Some("archived-v1"), ServeAttribution::Profile),
    );
    assert_eq!(counts(&state), (0, 0));
    assert_eq!(state.balanced_recall.total_events, 0);
    assert_eq!(state.profile_states["archived-v1"].total_events, 0);
}

#[test]
fn a_named_profile_that_is_not_resident_counts_nothing() {
    let mut state = BrainState::new(8);
    crate::apply_dispatch_signal(
        &mut state,
        &hit(Some("absent-v1"), ServeAttribution::Profile),
    );
    assert_eq!(counts(&state), (0, 0));
    let mut control = BrainState::new(8);
    control
        .profile_states
        .insert("absent-v1".to_string(), BalancedRecallState::new(8));
    crate::apply_dispatch_signal(
        &mut control,
        &hit(Some("absent-v1"), ServeAttribution::Profile),
    );
    assert_eq!(
        counts(&control),
        (1, 0),
        "same signal, profile resident: the arm above measured residency, not the signal"
    );
}

#[test]
fn many_signals_serialize_once_and_the_value_is_what_the_eager_path_wrote() {
    let mut state = BrainState::new(8);
    for _ in 0..20 {
        crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unspecified));
    }
    // What the eager path produced for this signal sequence, computed the
    // way it computed it.
    let expected =
        serde_json::to_value(state.balanced_recall.to_snapshot()).expect("live state serializes");
    let record = state
        .materialized_profile("balanced-recall-v1")
        .expect("the default profile is registered");
    assert_eq!(
        record.state_snapshot.as_ref(),
        Some(&expected),
        "a lazy snapshot that differs from the eager one is a different feature"
    );
    assert_eq!(
        record.state_snapshot.map(|v| v.to_string()),
        Some(expected.to_string()),
        "byte-identical, not merely equal as values"
    );
    assert_eq!(
        counts(&state),
        (20, 1),
        "twenty signals, one read, one serialization"
    );
}

#[test]
fn the_persisted_snapshot_is_materialized_and_the_counters_stay_out_of_it() {
    let mut state = BrainState::new(8);
    for _ in 0..4 {
        crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unspecified));
    }
    let expected =
        serde_json::to_value(state.balanced_recall.to_snapshot()).expect("live state serializes");
    let snapshot = state.to_snapshot();
    assert_eq!(
        snapshot.profiles["balanced-recall-v1"]
            .state_snapshot
            .as_ref(),
        Some(&expected),
        "persistence must not write the stale value the signal path stopped updating"
    );
    let persisted = serde_json::to_value(&snapshot).expect("snapshot serializes");
    assert!(
        persisted.get("signals_applied").is_none()
            && persisted.get("snapshot_serializations").is_none(),
        "process-local diagnostics must not enter the durable shape: {persisted}"
    );
    assert!(
        persisted.get("balanced_recall").is_some(),
        "control: the snapshot really was produced"
    );
}

#[tokio::test]
async fn brain_state_reports_the_counters_beside_the_state() {
    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let pack = crate::BrainPack::new(rt);
    {
        let mut state = pack.state.lock().unwrap();
        for _ in 0..2 {
            crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unspecified));
        }
    }
    let value = pack
        .handle_state(json!({}))
        .await
        .expect("brain.state must answer");
    assert_eq!(value["dispatch_counters"]["signals_applied"], 2);
    assert!(
        value.get("balanced_recall").is_some(),
        "control: the state itself is still reported"
    );
}

#[tokio::test]
async fn the_profile_verb_serves_a_snapshot_the_signal_path_never_wrote() {
    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let pack = crate::BrainPack::new(rt);
    let expected = {
        let mut state = pack.state.lock().unwrap();
        for _ in 0..6 {
            crate::apply_dispatch_signal(&mut state, &hit(None, ServeAttribution::Unspecified));
        }
        assert_eq!(
            state.snapshot_serializations.load(Ordering::Relaxed),
            0,
            "control: nothing serialized before the read"
        );
        serde_json::to_value(state.balanced_recall.to_snapshot()).expect("live state")
    };
    let value = pack
        .handle_profile(json!({ "profile_id": "balanced-recall-v1" }))
        .await
        .expect("brain.profile must answer");
    assert_eq!(value["state_snapshot"], expected);
    assert_eq!(value["total_events"], 6);
    let state = pack.state.lock().unwrap();
    assert_eq!(state.snapshot_serializations.load(Ordering::Relaxed), 1);
}
