use super::*;
use khive_db::StorageBackend;

fn make_backend() -> StorageBackend {
    let backend = StorageBackend::memory().expect("memory backend");
    backend.prepare_core_schema().expect("core schema");
    backend
}

// -------------------------------------------------------------------------
// Test 1 — ambient channel drives weight above 1.0 over 5 ticks
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_apply_weight_delta_ambient_channel() {
    let conn = make_backend();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();
    let delta = 0.01_f32; // positive ambient nudge

    let mut last_weight = 1.0_f32;
    for _ in 0..5 {
        let (w, _row_id) = apply_weight_delta(
            &conn,
            lambda,
            atom,
            delta,
            WeightChannel::Ambient,
            None,
            None,
        )
        .await
        .expect("apply_weight_delta should succeed");
        last_weight = w;
    }

    assert!(
        last_weight > 1.0,
        "weight should rise above 1.0 with positive delta, got {last_weight}"
    );
    assert!(
        last_weight < WEIGHT_CEIL,
        "weight should not reach ceiling after 5 ticks"
    );

    // Verify 5 audit rows were written.
    let map = batch_load_weights(&conn, lambda, &[atom])
        .await
        .expect("batch_load_weights");
    assert!(map.contains_key(&atom), "weight row should exist");

    // Count weight_events rows directly.
    let count = conn
            .reader()
            .await
            .expect("reader")
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM weight_events WHERE namespace = ?1 AND atom_id = ?2 AND channel = 'ambient'",
                vec![SqlValue::Text(lambda.into()), SqlValue::Text(atom.to_string())],
            ))
            .await
            .expect("audit count");
    assert_eq!(count, 5, "expected 5 ambient weight_events rows");
}

// -------------------------------------------------------------------------
// Test 2 — ceiling clamp
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_apply_weight_delta_clamps_at_ceiling() {
    let conn = make_backend();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();

    // Repeatedly push large positive deltas.
    for _ in 0..100 {
        apply_weight_delta(
            &conn,
            lambda,
            atom,
            5.0, // huge delta
            WeightChannel::GroundTruth,
            None,
            None,
        )
        .await
        .expect("apply_weight_delta should succeed");
    }

    let map = batch_load_weights(&conn, lambda, &[atom])
        .await
        .expect("batch_load_weights");
    let w = *map.get(&atom).expect("atom weight must exist");
    assert_eq!(
        w, WEIGHT_CEIL,
        "weight should be clamped at WEIGHT_CEIL={WEIGHT_CEIL}, got {w}"
    );
}

// -------------------------------------------------------------------------
// Test 3 — namespace isolation
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_apply_weight_delta_namespace_isolation() {
    let conn = make_backend();
    let lambda_a = "lambda:a";
    let lambda_b = "lambda:b";
    let atom = Uuid::new_v4();

    apply_weight_delta(
        &conn,
        lambda_a,
        atom,
        0.5,
        WeightChannel::Explicit,
        None,
        None,
    )
    .await
    .expect("apply for lambda:a");

    // lambda:b should see nothing.
    let map_b = batch_load_weights(&conn, lambda_b, &[atom])
        .await
        .expect("batch_load for lambda:b");
    assert!(
        !map_b.contains_key(&atom),
        "lambda:b should not see lambda:a's weight"
    );

    // lambda:a should see the written weight.
    let map_a = batch_load_weights(&conn, lambda_a, &[atom])
        .await
        .expect("batch_load for lambda:a");
    assert!(
        map_a.contains_key(&atom),
        "lambda:a should see its own weight"
    );
}

// -------------------------------------------------------------------------
// Test 4 — missing atoms are absent (not 1.0 rows) from batch_load result
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_batch_load_weights_missing_atoms_default() {
    let conn = make_backend();
    let lambda = "lambda:test";
    let atom_a = Uuid::new_v4();
    let atom_b = Uuid::new_v4();
    let atom_c = Uuid::new_v4();

    // Write only atom_a.
    apply_weight_delta(
        &conn,
        lambda,
        atom_a,
        0.3,
        WeightChannel::Explicit,
        None,
        None,
    )
    .await
    .expect("apply for atom_a");

    let map = batch_load_weights(&conn, lambda, &[atom_a, atom_b, atom_c])
        .await
        .expect("batch_load");

    assert!(map.contains_key(&atom_a), "atom_a should be present");
    assert!(
        !map.contains_key(&atom_b),
        "atom_b should be absent (caller treats as 1.0)"
    );
    assert!(
        !map.contains_key(&atom_c),
        "atom_c should be absent (caller treats as 1.0)"
    );

    // atom_a weight should be non-default (was boosted).
    let w_a = *map.get(&atom_a).unwrap();
    assert!(w_a != 1.0_f32, "atom_a weight should differ from default");
}

// -------------------------------------------------------------------------
// Test 5 — negative delta writes a weight_events row (B4 regression guard)
// -------------------------------------------------------------------------
/// Verifies that apply_weight_delta writes to weight_events even when the
/// delta is negative, guarding against the B4 Channel-A decay skip bug.
#[tokio::test]
async fn test_channel_a_applies_on_negative_delta() {
    let conn = make_backend();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();

    // First boost the atom so it is above floor.
    apply_weight_delta(
        &conn,
        lambda,
        atom,
        0.5,
        WeightChannel::GroundTruth,
        None,
        None,
    )
    .await
    .expect("initial boost");

    // Now apply a negative ambient delta (simulates decay).
    let (w_after, _) = apply_weight_delta(
        &conn,
        lambda,
        atom,
        -0.1,
        WeightChannel::Ambient,
        None,
        Some("decay_test"),
    )
    .await
    .expect("negative delta must succeed");

    // Weight must be below the post-boost value (started ~1.25, decay should lower it).
    assert!(
        w_after < 1.5,
        "weight should have decayed below post-boost value, got {w_after}"
    );
    assert!(
        w_after >= WEIGHT_FLOOR,
        "weight must not go below WEIGHT_FLOOR, got {w_after}"
    );

    // Confirm a weight_events row was written for the negative delta.
    let count = conn
        .reader()
        .await
        .expect("reader")
        .count(SqlStatement::new(
            "SELECT COUNT(*) FROM weight_events \
                 WHERE namespace = ?1 AND atom_id = ?2 AND delta < 0",
            vec![
                SqlValue::Text(lambda.into()),
                SqlValue::Text(atom.to_string()),
            ],
        ))
        .await
        .expect("negative audit count");
    assert_eq!(
        count, 1,
        "expected 1 weight_event row with negative delta, got {count}"
    );
}

// -------------------------------------------------------------------------
// Test 6 — empty namespace returns Validation error (F2 guard)
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_apply_weight_delta_rejects_empty_namespace() {
    let conn = make_backend();
    let atom = Uuid::new_v4();
    let result = apply_weight_delta(&conn, "", atom, 0.1, WeightChannel::Ambient, None, None).await;
    assert!(
        matches!(result, Err(PersistError::Validation(_))),
        "expected Validation error for empty namespace, got {result:?}"
    );
}

// -------------------------------------------------------------------------
// Test 7 — NaN delta returns Validation error (prevents DB corruption)
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_apply_weight_delta_rejects_nan_delta() {
    let conn = make_backend();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();

    let result = apply_weight_delta(
        &conn,
        lambda,
        atom,
        f32::NAN,
        WeightChannel::Ambient,
        None,
        None,
    )
    .await;
    assert!(
        matches!(result, Err(PersistError::Validation(_))),
        "NaN delta must be rejected with Validation error, got {result:?}"
    );
}

// -------------------------------------------------------------------------
// Test 8 — Inf delta returns Validation error
// -------------------------------------------------------------------------
#[tokio::test]
async fn test_apply_weight_delta_rejects_inf_delta() {
    let conn = make_backend();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();

    let result = apply_weight_delta(
        &conn,
        lambda,
        atom,
        f32::INFINITY,
        WeightChannel::Ambient,
        None,
        None,
    )
    .await;
    assert!(
        matches!(result, Err(PersistError::Validation(_))),
        "Inf delta must be rejected with Validation error, got {result:?}"
    );
}
