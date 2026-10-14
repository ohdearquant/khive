use uuid::Uuid;

use super::*;
use khive_brain_core::BetaPosterior;
use khive_runtime::{KhiveRuntime, Namespace};

/// Oversized persisted snapshot restore is bounded/rejected: a snapshot
/// with more entity posteriors than `ENTITY_CACHE_CAPACITY` must fail
/// capacity-aware validation at the load boundary rather than silently
/// restoring past the configured bound.
#[tokio::test]
async fn oversized_brain_snapshot_load_returns_runtime_error() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let capacity = 2;
    let mut state = BrainState::new(capacity);
    for _ in 0..capacity {
        state
            .balanced_recall
            .entity_posteriors
            .get_or_insert(Uuid::new_v4(), BetaPosterior::default);
    }
    let mut snapshot = state.to_snapshot();
    // Force one entry past the configured capacity — simulates a crafted
    // or legacy oversized snapshot that live inserts could never produce.
    snapshot
        .balanced_recall
        .entity_posteriors
        .insert(Uuid::new_v4(), BetaPosterior::default());

    upsert_snapshot(sql.as_ref(), ns, &snapshot, 500_000)
        .await
        .expect("seed oversized snapshot");

    let result = load_latest_snapshot(sql.as_ref(), ns, capacity).await;
    assert!(
        result.is_err(),
        "snapshot with entity_posteriors.len() > capacity must be rejected at load"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("snapshot invariant violation"),
        "error must name the invariant-violation load boundary, got: {err}"
    );
}

/// A snapshot within the configured capacity restores without error.
#[tokio::test]
async fn in_capacity_brain_snapshot_load_succeeds() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let capacity = 2;
    let state = BrainState::new(capacity);
    let snapshot = state.to_snapshot();
    upsert_snapshot(sql.as_ref(), ns, &snapshot, 500_000)
        .await
        .expect("seed snapshot");

    let result = load_latest_snapshot(sql.as_ref(), ns, capacity).await;
    assert!(result.is_ok(), "in-capacity snapshot must load cleanly");
}

/// PR #535: a persisted snapshot with `{A, B}`
/// entity_posteriors, `entity_posterior_order = [A]`, and an EXPLICIT
/// `entity_posteriors_version = 0` must be rejected at the
/// `load_latest_snapshot` boundary rather than silently normalized by
/// `restore` to `[A, B]`. Version 0 with a non-empty order is not the
/// legacy empty-order compatibility case.
#[tokio::test]
async fn version_zero_snapshot_with_explicit_partial_order_rejected_at_load() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let capacity = 10;
    let mut state = BrainState::new(capacity);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    state
        .balanced_recall
        .entity_posteriors
        .get_or_insert(a, BetaPosterior::default);
    state
        .balanced_recall
        .entity_posteriors
        .get_or_insert(b, BetaPosterior::default);

    let mut snapshot = state.to_snapshot();
    snapshot.balanced_recall.entity_posteriors_version = 0;
    snapshot.balanced_recall.entity_posterior_order = vec![a];

    upsert_snapshot(sql.as_ref(), ns, &snapshot, 500_000)
        .await
        .expect("seed corrupt version-0 snapshot");

    let result = load_latest_snapshot(sql.as_ref(), ns, capacity).await;
    assert!(
        result.is_err(),
        "version-0 snapshot with non-empty partial order must be rejected at load, not normalized"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("snapshot invariant violation"),
        "error must name the load-boundary invariant violation, got: {err}"
    );
}

/// Same corruption as above, but with `entity_posteriors_version` OMITTED
/// from the stored JSON entirely (rather than set to an explicit `0`) —
/// the `#[serde(default)]` path that made this reachable in the first
/// place. Inserted as a raw row (bypassing the typed `upsert_snapshot`,
/// which always serializes the full struct) to reproduce exactly what a
/// pre-versioning writer, or a hand-crafted snapshot, would have stored.
#[tokio::test]
async fn version_omitted_snapshot_with_partial_order_rejected_at_load() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let capacity = 10;
    let mut state = BrainState::new(capacity);
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    state
        .balanced_recall
        .entity_posteriors
        .get_or_insert(a, BetaPosterior::default);
    state
        .balanced_recall
        .entity_posteriors
        .get_or_insert(b, BetaPosterior::default);

    let mut snapshot = state.to_snapshot();
    snapshot.balanced_recall.entity_posterior_order = vec![a];

    let mut json = serde_json::to_value(&snapshot).expect("serialize snapshot");
    json["balanced_recall"]
        .as_object_mut()
        .expect("balanced_recall is an object")
        .remove("entity_posteriors_version");
    let json_str = serde_json::to_string(&json).expect("re-serialize snapshot");

    // Round-trip through the typed struct to confirm the omitted field
    // really does serde-default to 0 before we bypass the typed insert
    // path — this is the precondition the fix closes.
    let reparsed: BrainStateSnapshot =
        serde_json::from_str(&json_str).expect("deserialize snapshot with omitted version");
    assert_eq!(
        reparsed.balanced_recall.entity_posteriors_version, 0,
        "omitted entity_posteriors_version must serde-default to 0"
    );

    let mut writer = sql.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO brain_profile_snapshots (profile_id, namespace, snapshot_json, updated_at) VALUES (?1, ?2, ?3, ?4)".into(),
                params: vec![
                    SqlValue::Text(SNAPSHOT_PROFILE_ID.to_string()),
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(json_str),
                    SqlValue::Integer(500_000),
                ],
                label: None,
            })
            .await
            .expect("insert raw snapshot row with omitted version");
    drop(writer);

    let result = load_latest_snapshot(sql.as_ref(), ns, capacity).await;
    assert!(
            result.is_err(),
            "snapshot with omitted entity_posteriors_version and non-empty partial order must be rejected at load"
        );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("snapshot invariant violation"),
        "error must name the load-boundary invariant violation, got: {err}"
    );
}
