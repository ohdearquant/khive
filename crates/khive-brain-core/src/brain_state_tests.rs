use chrono::{TimeZone, Utc};

use super::*;
use crate::profile::ProfileLifecycle;

fn make_profile(id: &str, consumer_kind: &str, ts_secs: i64) -> ProfileRecord {
    ProfileRecord {
        id: id.to_owned(),
        description: String::new(),
        consumer_kind: consumer_kind.to_owned(),
        state_class: "Bayesian".into(),
        lifecycle: ProfileLifecycle::Active,
        created_at: Utc.timestamp_opt(ts_secs, 0).unwrap(),
        state_snapshot: None,
        total_events: 0,
        exploration_epoch: 0,
    }
}

/// `sort_fallback_candidates` produces `(created_at ASC, id ASC)` order.
/// See crates/khive-brain-core/docs/testing-strategy.md#brain_staters-sort_fallback_candidates_produces_created_at_then_id_order
#[test]
fn sort_fallback_candidates_produces_created_at_then_id_order() {
    // Three profiles inserted in DESCENDING created_at order (worst case for
    // any unsorted implementation).
    let p1 = make_profile("aardvark", "recall", 1_000); // earliest, lowest id
    let p2 = make_profile("mango", "recall", 2_000);
    let p3 = make_profile("zebra", "recall", 3_000); // latest, highest id

    // Intentionally unsorted: reverse order.
    let mut candidates = vec![&p3, &p2, &p1];
    sort_fallback_candidates(&mut candidates);

    // After sort: p1 (earliest) must be first, p3 (latest) must be last.
    assert_eq!(
        candidates[0].id, "aardvark",
        "earliest profile must come first"
    );
    assert_eq!(candidates[1].id, "mango");
    assert_eq!(candidates[2].id, "zebra", "latest profile must come last");
}

/// `sort_fallback_candidates` must break ties by `id` (ascending) when
/// `created_at` is equal.
#[test]
fn sort_fallback_candidates_breaks_ties_by_id() {
    let ts = 5_000i64;
    let p_z = make_profile("z-profile", "recall", ts);
    let p_a = make_profile("a-profile", "recall", ts);
    let p_m = make_profile("m-profile", "recall", ts);

    // Deliberately insert in reverse alphabetical order.
    let mut candidates = vec![&p_z, &p_m, &p_a];
    sort_fallback_candidates(&mut candidates);

    assert_eq!(
        candidates[0].id, "a-profile",
        "lowest id must come first on equal timestamps"
    );
    assert_eq!(candidates[1].id, "m-profile");
    assert_eq!(candidates[2].id, "z-profile");
}

/// `BrainState::resolve` selects the earliest-created profile regardless
/// of HashMap insertion order (integration guard on `sort_fallback_candidates`).
#[test]
fn resolve_fallback_is_deterministic() {
    let p_early = make_profile("alpha", "recall", 1_000);
    let p_later = make_profile("zeta", "recall", 2_000);

    let mut state_a = BrainState {
        profiles: HashMap::new(),
        balanced_recall: BalancedRecallState::new(8),
        profile_states: HashMap::new(),
        bindings: Vec::new(),
        section_states: HashMap::new(),
        router_state: HashMap::new(),
        adapter_set: HashMap::new(),
        signals_applied: AtomicU64::new(0),
        snapshot_serializations: AtomicU64::new(0),
    };
    state_a.profiles.insert(p_early.id.clone(), p_early.clone());
    state_a.profiles.insert(p_later.id.clone(), p_later.clone());

    let mut state_b = BrainState {
        profiles: HashMap::new(),
        balanced_recall: BalancedRecallState::new(8),
        profile_states: HashMap::new(),
        bindings: Vec::new(),
        section_states: HashMap::new(),
        router_state: HashMap::new(),
        adapter_set: HashMap::new(),
        signals_applied: AtomicU64::new(0),
        snapshot_serializations: AtomicU64::new(0),
    };
    // Insert in the opposite order.
    state_b.profiles.insert(p_later.id.clone(), p_later.clone());
    state_b.profiles.insert(p_early.id.clone(), p_early.clone());

    let result_a = state_a.resolve(None, None, "recall");
    let result_b = state_b.resolve(None, None, "recall");

    let id_a = result_a.map(|p| p.id.clone());
    let id_b = result_b.map(|p| p.id.clone());
    assert_eq!(id_a, Some("alpha".to_owned()));
    assert_eq!(id_a, id_b, "fallback resolution must be deterministic");
}

/// `router_state` and `adapter_set` survive a snapshot round-trip byte-for-byte.
#[test]
fn router_state_and_adapter_set_round_trip() {
    let mut state = BrainState::new(8);

    let blob = RouterStateBlob {
        schema_version: 1,
        gate_bytes: vec![1, 2, 3],
    };
    state
        .router_state
        .insert("profile-x".to_owned(), blob.clone());

    let record = AdapterRecord {
        adapter_id: "lora-42".to_owned(),
        slot: 0,
        content_hash: "abc123".to_owned(),
    };
    state
        .adapter_set
        .insert("profile-x".to_owned(), vec![record.clone()]);

    let snapshot = state.to_snapshot();
    let restored = BrainState::from_snapshot(snapshot, 8);

    assert_eq!(
        restored.router_state.get("profile-x"),
        Some(&blob),
        "router_state must survive round-trip"
    );
    assert_eq!(
        restored.adapter_set.get("profile-x"),
        Some(&vec![record]),
        "adapter_set must survive round-trip"
    );
}

/// Omitting `router_state`/`adapter_set` from JSON must deserialize to
/// empty maps via `#[serde(default)]`, with no migration required.
#[test]
fn snapshot_missing_router_and_adapter_fields_defaults_to_empty() {
    let state = BrainState::new(8);
    let snapshot = state.to_snapshot();

    let mut value: serde_json::Value = serde_json::to_value(&snapshot).expect("serialize snapshot");

    let obj = value.as_object_mut().expect("snapshot is a JSON object");
    obj.remove("router_state");
    obj.remove("adapter_set");

    let restored: BrainStateSnapshot =
        serde_json::from_value(value).expect("deserialize old-format snapshot");

    assert!(
        restored.router_state.is_empty(),
        "router_state must default to empty map when absent from JSON"
    );
    assert!(
        restored.adapter_set.is_empty(),
        "adapter_set must default to empty map when absent from JSON"
    );
}

/// BRAINCORE-AUD-001 regression: oversized-snapshot restore is rejected
/// (bounded/rejected) at the capacity-aware validation boundary.
#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_entity_posteriors_over_capacity() {
    use uuid::Uuid;

    let capacity = 2;
    let mut state = BrainState::new(capacity);
    for _ in 0..(capacity + 1) {
        state
            .balanced_recall
            .entity_posteriors
            .get_or_insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default);
    }
    // Force an over-capacity snapshot directly, since live inserts already
    // enforce the bound — this simulates a crafted/legacy oversized snapshot.
    let mut snapshot = state.to_snapshot();
    snapshot
        .balanced_recall
        .entity_posteriors
        .insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default());

    let result = validate_brain_state_snapshot_with_capacity(&snapshot, capacity);
    assert!(
        result.is_err(),
        "snapshot with entity_posteriors.len() > capacity must be rejected"
    );
}

/// BRAINCORE-AUD-001: capacity violation in a named `profile_states`
/// entry (not just the default profile) must also be rejected.
#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_profile_state_over_capacity() {
    use uuid::Uuid;

    let capacity = 1;
    let mut state = BrainState::new(capacity);
    let mut extra = BalancedRecallState::new(capacity);
    extra
        .entity_posteriors
        .get_or_insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default);
    state
        .profile_states
        .insert("extra-profile".to_owned(), extra);

    let mut snapshot = state.to_snapshot();
    snapshot
        .profile_states
        .get_mut("extra-profile")
        .unwrap()
        .entity_posteriors
        .insert(Uuid::new_v4(), crate::posterior::BetaPosterior::default());

    let result = validate_brain_state_snapshot_with_capacity(&snapshot, capacity);
    assert!(
        result.is_err(),
        "profile_states entry with entity_posteriors.len() > capacity must be rejected"
    );
}

// ── entity_posterior_order strict coverage regression guard (PR #535) ──────────────

/// Build a fresh version-1 `BalancedRecallSnapshot` with `n` entity
/// posteriors and a fully-covering order, for mutation in the tests below.
fn make_versioned_recall_snapshot(capacity: usize, n: usize) -> BalancedRecallSnapshot {
    let mut state = BalancedRecallState::new(capacity);
    for _ in 0..n {
        state.entity_posteriors.get_or_insert(
            uuid::Uuid::new_v4(),
            crate::posterior::BetaPosterior::default,
        );
    }
    state.to_snapshot()
}

#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_missing_order_ids() {
    let capacity = 4;
    let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
    assert_eq!(snapshot.entity_posteriors_version, 1);
    assert_eq!(snapshot.entity_posterior_order.len(), 2);

    // Drop one id from the order — partial coverage on a current-format
    // (version 1) snapshot must be rejected, not silently normalized.
    snapshot.entity_posterior_order.truncate(1);

    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = snapshot;

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_err(),
        "version-1 snapshot with order shorter than entity_posteriors must be rejected"
    );
    assert!(
        result.unwrap_err().contains("does not cover all"),
        "error must name the missing-coverage reason"
    );
}

#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_duplicate_order_ids() {
    let capacity = 4;
    let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
    let dup = snapshot.entity_posterior_order[0];
    snapshot.entity_posterior_order[1] = dup;

    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = snapshot;

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_err(),
        "duplicate entity_posterior_order ids must be rejected"
    );
    assert!(result.unwrap_err().contains("duplicate id"));
}

#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_unknown_order_ids() {
    let capacity = 4;
    let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
    snapshot.entity_posterior_order.push(uuid::Uuid::new_v4());

    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = snapshot;

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_err(),
        "entity_posterior_order id absent from entity_posteriors must be rejected"
    );
    assert!(result
        .unwrap_err()
        .contains("not present in entity_posteriors"));
}

#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_unknown_version() {
    let capacity = 4;
    let mut snapshot = make_versioned_recall_snapshot(capacity, 1);
    snapshot.entity_posteriors_version = 2;

    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = snapshot;

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_err(),
        "unknown entity_posteriors_version must be rejected"
    );
    assert!(result.unwrap_err().contains("unknown version"));
}

#[test]
fn validate_brain_state_snapshot_with_capacity_accepts_legacy_empty_order() {
    let capacity = 4;
    let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
    // Explicitly-legacy version 0 with no order metadata must still pass —
    // restore falls back to the deterministic ascending-Uuid path.
    snapshot.entity_posteriors_version = 0;
    snapshot.entity_posterior_order.clear();

    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = snapshot;

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_ok(),
        "legacy version-0 snapshot with empty order must be accepted: {result:?}"
    );
}

#[test]
fn validate_brain_state_snapshot_with_capacity_rejects_version_zero_with_partial_order() {
    // PR #535: version-0 with a partial order is corruption, not legacy
    // compatibility data — see design-notes.md.
    let capacity = 4;
    let mut snapshot = make_versioned_recall_snapshot(capacity, 2);
    snapshot.entity_posteriors_version = 0;
    snapshot.entity_posterior_order.truncate(1);

    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = snapshot;

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_err(),
        "version-0 snapshot with non-empty partial order must be rejected"
    );
    assert!(
        result
            .unwrap_err()
            .contains("requires entity_posteriors_version >= 1"),
        "error must name the version-0-with-non-empty-order reason"
    );
}

#[test]
fn validate_brain_state_snapshot_with_capacity_accepts_full_order_current_version() {
    let capacity = 4;
    let mut full_snapshot = BrainState::new(capacity).to_snapshot();
    full_snapshot.balanced_recall = make_versioned_recall_snapshot(capacity, 3);

    let result = validate_brain_state_snapshot_with_capacity(&full_snapshot, capacity);
    assert!(
        result.is_ok(),
        "version-1 snapshot with a fully-covering order must be accepted: {result:?}"
    );
}

/// A validated snapshot must restore→re-snapshot→restore to the same
/// state — the validation gate must not itself introduce drift.
#[test]
fn restore_to_snapshot_restore_idempotency() {
    let capacity = 4;
    let mut state = BrainState::new(capacity);
    for _ in 0..3 {
        state.balanced_recall.entity_posteriors.get_or_insert(
            uuid::Uuid::new_v4(),
            crate::posterior::BetaPosterior::default,
        );
    }

    let snapshot_1 = state.to_snapshot();
    validate_brain_state_snapshot_with_capacity(&snapshot_1, capacity)
        .expect("first snapshot must validate");

    let restored_1 = BrainState::from_snapshot(snapshot_1, capacity);
    let snapshot_2 = restored_1.to_snapshot();
    validate_brain_state_snapshot_with_capacity(&snapshot_2, capacity)
        .expect("round-tripped snapshot must still validate");

    let restored_2 = BrainState::from_snapshot(snapshot_2.clone(), capacity);
    let snapshot_3 = restored_2.to_snapshot();

    assert_eq!(
        snapshot_2.balanced_recall.entity_posteriors, snapshot_3.balanced_recall.entity_posteriors,
        "entity_posteriors must be stable across a second restore/snapshot cycle"
    );
    assert_eq!(
        snapshot_2.balanced_recall.entity_posterior_order,
        snapshot_3.balanced_recall.entity_posterior_order,
        "entity_posterior_order must be stable across a second restore/snapshot cycle"
    );
}
