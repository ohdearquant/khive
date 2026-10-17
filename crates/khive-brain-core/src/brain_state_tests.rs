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

include!("brain_state_tests/fallback_resolution_tests.rs");

include!("brain_state_tests/snapshot_values_tests.rs");

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
