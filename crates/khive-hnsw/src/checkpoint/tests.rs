#![allow(clippy::field_reassign_with_default)]

use super::*;

fn make_id(seed: u8) -> NodeId {
    NodeId::new([seed; 16])
}

fn sample_config() -> HnswCheckpointConfig {
    HnswCheckpointConfig {
        m: 16,
        ef_construction: 200,
        metric: "cosine".to_string(),
    }
}

fn sample_snapshot() -> HnswSnapshot {
    let id1 = make_id(1);
    let id2 = make_id(2);
    HnswSnapshot {
        vector_count: 0, // Not used in v2
        total_nodes: 2,
        live_nodes: 2,
        tombstone_count: 0,
        max_layer: 1,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2],
        tombstoned_ids: vec![],
        layers: vec![
            // Layer 0: both nodes connected to each other
            vec![(id1, vec![id2]), (id2, vec![id1])],
            // Layer 1: only entry point
            vec![(id1, vec![])],
        ],
        vectors: vec![],
    }
}

fn sample_snapshot_with_tombstones() -> HnswSnapshot {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 2,
        tombstone_count: 1,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2, id3],
        tombstoned_ids: vec![id2], // id2 is tombstoned
        layers: vec![vec![
            (id1, vec![id2, id3]),
            (id2, vec![id1]),
            (id3, vec![id1]),
        ]],
        vectors: vec![],
    }
}

#[test]
fn snapshot_creation_and_accessors() {
    let snap = sample_snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap.total_len(), 2);
    assert_eq!(snap.tombstone_count(), 0);
    assert!(!snap.is_empty());
    assert_eq!(snap.max_layer, 1);
    assert!(snap.entry_point.is_some());
    assert_eq!(snap.indexed_ids.len(), 2);
    assert_eq!(snap.layers.len(), 2);
}

#[test]
fn snapshot_with_tombstones_accessors() {
    let snap = sample_snapshot_with_tombstones();
    assert_eq!(snap.len(), 2); // live nodes
    assert_eq!(snap.total_len(), 3); // total including tombstones
    assert_eq!(snap.tombstone_count(), 1);
    assert!(!snap.is_empty());
}

#[test]
fn empty_snapshot() {
    let snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 0,
        live_nodes: 0,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: None,
        config: sample_config(),
        indexed_ids: vec![],
        tombstoned_ids: vec![],
        layers: vec![],

        vectors: vec![],
    };
    assert!(snap.is_empty());
    assert_eq!(snap.len(), 0);
    assert_eq!(snap.total_len(), 0);
}

// ── Verification tests ───────────────────────────────────────────────

#[test]
fn verify_valid_snapshot() {
    let snap = sample_snapshot();
    assert!(snap.verify().is_ok());
}

#[test]
fn verify_valid_snapshot_with_tombstones() {
    let snap = sample_snapshot_with_tombstones();
    assert!(snap.verify().is_ok());
}

#[test]
fn verify_inconsistent_counts() {
    let mut snap = sample_snapshot();
    snap.tombstone_count = 1; // Inconsistent: 2 != 2 + 1
    let err = snap.verify().unwrap_err();
    assert!(matches!(err, SnapshotError::InconsistentCounts { .. }));
}

#[test]
fn verify_id_count_mismatch() {
    let mut snap = sample_snapshot();
    snap.total_nodes = 5; // Mismatch with indexed_ids.len() == 2
    snap.live_nodes = 5;
    let err = snap.verify().unwrap_err();
    assert!(matches!(err, SnapshotError::IdCountMismatch { .. }));
}

#[test]
fn verify_tombstone_id_count_mismatch() {
    let mut snap = sample_snapshot_with_tombstones();
    snap.tombstone_count = 2; // Mismatch with tombstoned_ids.len() == 1
    snap.live_nodes = 1; // Adjust to keep total consistent
    let err = snap.verify().unwrap_err();
    assert!(matches!(
        err,
        SnapshotError::TombstoneIdCountMismatch { .. }
    ));
}

#[test]
fn verify_tombstone_not_in_index() {
    let mut snap = sample_snapshot_with_tombstones();
    snap.tombstoned_ids = vec![make_id(99)]; // ID not in indexed_ids
    let err = snap.verify().unwrap_err();
    assert!(matches!(err, SnapshotError::TombstoneNotInIndex { .. }));
}

// ── Normalization tests ──────────────────────────────────────────────

#[test]
fn normalize_v1_snapshot() {
    // Simulate a v1 snapshot with only vector_count
    let mut snap = HnswSnapshot {
        vector_count: 5, // V1 field
        total_nodes: 0,  // Will be populated by normalize
        live_nodes: 0,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: None,
        config: sample_config(),
        indexed_ids: vec![make_id(1), make_id(2), make_id(3), make_id(4), make_id(5)],
        tombstoned_ids: vec![],
        layers: vec![],

        vectors: vec![],
    };

    snap.normalize();

    assert_eq!(snap.total_nodes, 5);
    assert_eq!(snap.live_nodes, 5);
    assert_eq!(snap.tombstone_count, 0);
}

#[test]
fn normalize_infers_from_indexed_ids() {
    let mut snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 0,
        live_nodes: 0,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: None,
        config: sample_config(),
        indexed_ids: vec![make_id(1), make_id(2), make_id(3)],
        tombstoned_ids: vec![make_id(2)],
        layers: vec![],

        vectors: vec![],
    };

    snap.normalize();

    assert_eq!(snap.total_nodes, 3);
    assert_eq!(snap.live_nodes, 2);
    assert_eq!(snap.tombstone_count, 1);
}

// ── Serialization tests ──────────────────────────────────────────────

#[test]
fn serialization_round_trip() {
    let snap = sample_snapshot();
    let json = serde_json::to_string(&snap).expect("serialize");
    let mut restored: HnswSnapshot = serde_json::from_str(&json).expect("deserialize");
    restored.normalize();

    assert_eq!(restored.total_nodes, snap.total_nodes);
    assert_eq!(restored.live_nodes, snap.live_nodes);
    assert_eq!(restored.tombstone_count, snap.tombstone_count);
    assert_eq!(restored.max_layer, snap.max_layer);
    assert_eq!(restored.entry_point, snap.entry_point);
    assert_eq!(restored.config, snap.config);
    assert_eq!(restored.indexed_ids, snap.indexed_ids);
    assert_eq!(restored.tombstoned_ids, snap.tombstoned_ids);
    assert_eq!(restored.layers.len(), snap.layers.len());
}

#[test]
fn serialization_round_trip_with_tombstones() {
    let snap = sample_snapshot_with_tombstones();
    let json = serde_json::to_string(&snap).expect("serialize");
    let restored: HnswSnapshot = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(restored.total_nodes, snap.total_nodes);
    assert_eq!(restored.live_nodes, snap.live_nodes);
    assert_eq!(restored.tombstone_count, snap.tombstone_count);
    assert_eq!(restored.tombstoned_ids, snap.tombstoned_ids);
    assert!(restored.verify().is_ok());
}

#[test]
fn backward_compat_v1_deserialization() {
    // JSON from a v1 snapshot (only has vector_count, not new fields)
    // EmbeddingId serializes as 32 hex chars (no dashes)
    let v1_json = r#"{
        "vector_count": 2,
        "max_layer": 0,
        "entry_point": null,
        "config": {"m": 16, "ef_construction": 200, "metric": "cosine"},
        "indexed_ids": ["01010101010101010101010101010101", "02020202020202020202020202020202"],
        "layers": []
    }"#;

    let mut snap: HnswSnapshot = serde_json::from_str(v1_json).expect("deserialize v1");
    snap.normalize();

    assert_eq!(snap.total_nodes, 2);
    assert_eq!(snap.live_nodes, 2);
    assert_eq!(snap.tombstone_count, 0);
    assert_eq!(snap.len(), 2);
}

// ── Compatibility tests ──────────────────────────────────────────────

#[test]
fn compatibility_matching_config() {
    let snap = sample_snapshot();
    assert!(snap.is_compatible(&sample_config()));
}

#[test]
fn compatibility_different_m() {
    let snap = sample_snapshot();
    let other = HnswCheckpointConfig {
        m: 32,
        ..sample_config()
    };
    assert!(!snap.is_compatible(&other));
}

#[test]
fn compatibility_different_ef() {
    let snap = sample_snapshot();
    let other = HnswCheckpointConfig {
        ef_construction: 400,
        ..sample_config()
    };
    assert!(!snap.is_compatible(&other));
}

#[test]
fn compatibility_different_metric() {
    let snap = sample_snapshot();
    let other = HnswCheckpointConfig {
        metric: "euclidean".to_string(),
        ..sample_config()
    };
    assert!(!snap.is_compatible(&other));
}

#[test]
fn from_hnsw_config() {
    use super::super::config::HnswConfig;

    let hnsw = HnswConfig::default();
    let ckpt = HnswCheckpointConfig::from_hnsw_config(&hnsw);
    assert_eq!(ckpt.m, 20);
    assert_eq!(ckpt.ef_construction, 200);
    assert_eq!(ckpt.metric, "cosine");
}

#[test]
fn from_hnsw_config_variants() {
    use super::super::config::{DistanceMetric, HnswConfig};

    let mut config = HnswConfig::default();

    config.metric = DistanceMetric::Dot;
    assert_eq!(
        HnswCheckpointConfig::from_hnsw_config(&config).metric,
        "dot"
    );

    config.metric = DistanceMetric::L2;
    assert_eq!(
        HnswCheckpointConfig::from_hnsw_config(&config).metric,
        "euclidean"
    );
}

#[test]
fn metric_to_string_exhaustive() {
    assert_eq!(metric_to_string(&DistanceMetric::Cosine), "cosine");
    assert_eq!(metric_to_string(&DistanceMetric::Dot), "dot");
    assert_eq!(metric_to_string(&DistanceMetric::L2), "euclidean");
}

// ── Error display tests ──────────────────────────────────────────────

#[test]
fn snapshot_error_display() {
    let err = SnapshotError::InconsistentCounts {
        total: 5,
        live: 3,
        tombstones: 1,
    };
    assert!(err.to_string().contains("inconsistent counts"));

    let err = SnapshotError::IdCountMismatch {
        expected: 5,
        actual: 3,
    };
    assert!(err.to_string().contains("indexed_ids count mismatch"));

    let err = SnapshotError::TombstoneIdCountMismatch {
        expected: 2,
        actual: 1,
    };
    assert!(err.to_string().contains("tombstoned_ids count mismatch"));

    let err = SnapshotError::TombstoneNotInIndex { id: make_id(1) };
    assert!(err.to_string().contains("not found in indexed_ids"));
}

// ── Canonical ordering tests ──────────────────────────────────────────

#[test]
fn sort_ids_orders_by_bytes() {
    let mut ids = vec![make_id(5), make_id(2), make_id(9), make_id(1)];
    sort_ids(&mut ids);

    assert_eq!(ids[0], make_id(1));
    assert_eq!(ids[1], make_id(2));
    assert_eq!(ids[2], make_id(5));
    assert_eq!(ids[3], make_id(9));
}

#[test]
fn sort_ids_empty_is_noop() {
    let mut ids: Vec<NodeId> = vec![];
    sort_ids(&mut ids);
    assert!(ids.is_empty());
}

#[test]
fn sort_ids_single_element() {
    let mut ids = vec![make_id(42)];
    sort_ids(&mut ids);
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], make_id(42));
}

#[test]
fn is_canonical_sorted_snapshot() {
    // sample_snapshot() has ids in sorted order (1, 2) and layers also sorted
    let snap = sample_snapshot();
    assert!(snap.is_canonical());
}

#[test]
fn is_canonical_empty_snapshot() {
    let snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 0,
        live_nodes: 0,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: None,
        config: sample_config(),
        indexed_ids: vec![],
        tombstoned_ids: vec![],
        layers: vec![],

        vectors: vec![],
    };
    assert!(snap.is_canonical());
}

#[test]
fn is_canonical_unsorted_indexed_ids() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 2,
        live_nodes: 2,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id2, id1], // Reversed order
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![id2]), (id2, vec![id1])]],

        vectors: vec![],
    };
    assert!(!snap.is_canonical());
}

#[test]
fn is_canonical_unsorted_tombstoned_ids() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    let snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 1,
        tombstone_count: 2,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2, id3], // Sorted
        tombstoned_ids: vec![id3, id2],   // Reversed order
        layers: vec![],

        vectors: vec![],
    };
    assert!(!snap.is_canonical());
}

#[test]
fn is_canonical_unsorted_layer() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 2,
        live_nodes: 2,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2], // Sorted
        tombstoned_ids: vec![],
        layers: vec![vec![(id2, vec![id1]), (id1, vec![id2])]], // Reversed order

        vectors: vec![],
    };
    assert!(!snap.is_canonical());
}

#[test]
fn canonicalize_sorts_indexed_ids() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    let mut snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 3,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id3, id1, id2], // Unsorted
        tombstoned_ids: vec![],
        layers: vec![],

        vectors: vec![],
    };

    snap.canonicalize();

    assert_eq!(snap.indexed_ids, vec![id1, id2, id3]);
    assert!(snap.is_canonical());
}

#[test]
fn canonicalize_sorts_tombstoned_ids() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    let mut snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 1,
        tombstone_count: 2,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2, id3],
        tombstoned_ids: vec![id3, id2], // Unsorted
        layers: vec![],

        vectors: vec![],
    };

    snap.canonicalize();

    assert_eq!(snap.tombstoned_ids, vec![id2, id3]);
    assert!(snap.is_canonical());
}

#[test]
fn canonicalize_sorts_layers() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    let mut snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 3,
        tombstone_count: 0,
        max_layer: 1,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2, id3],
        tombstoned_ids: vec![],
        layers: vec![
            // Layer 0: nodes in wrong order
            vec![
                (id3, vec![id1, id2]),
                (id1, vec![id2, id3]),
                (id2, vec![id1, id3]),
            ],
            // Layer 1: also wrong order
            vec![(id2, vec![]), (id1, vec![])],
        ],

        vectors: vec![],
    };

    snap.canonicalize();

    // Verify layer 0 is sorted by node ID
    assert_eq!(snap.layers[0][0].0, id1);
    assert_eq!(snap.layers[0][1].0, id2);
    assert_eq!(snap.layers[0][2].0, id3);

    // Verify layer 1 is sorted by node ID
    assert_eq!(snap.layers[1][0].0, id1);
    assert_eq!(snap.layers[1][1].0, id2);

    assert!(snap.is_canonical());
}

#[test]
fn canonicalize_preserves_neighbor_order() {
    // Neighbor order should NOT be sorted (reflects proximity from HNSW algorithm)
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    let mut snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 3,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1, id2, id3],
        tombstoned_ids: vec![],
        layers: vec![vec![
            (id1, vec![id3, id2]), // Neighbors intentionally in non-byte-sorted order
        ]],

        vectors: vec![],
    };

    snap.canonicalize();

    // Neighbor list should be unchanged
    assert_eq!(snap.layers[0][0].1, vec![id3, id2]);
}

// ── Non-finite embedded vector boundary tests ────────────────────────────

/// TryFrom must reject a snapshot whose embedded vectors contain NaN.
#[test]
fn try_from_rejects_nan_in_embedded_vector() {
    use super::snapshot::RawHnswSnapshot;
    let id1 = make_id(1);
    let raw = RawHnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1],
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![])]],
        vectors: vec![(id1, vec![f32::NAN, 0.5])],
    };
    let result = HnswSnapshot::try_from(raw);
    assert!(
        matches!(result, Err(SnapshotError::NonFiniteVector { .. })),
        "TryFrom must reject embedded vector containing NaN"
    );
}

/// TryFrom must reject a snapshot whose embedded vectors contain Inf.
#[test]
fn try_from_rejects_inf_in_embedded_vector() {
    use super::snapshot::RawHnswSnapshot;
    let id1 = make_id(1);
    let raw = RawHnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1],
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![])]],
        vectors: vec![(id1, vec![0.5, f32::INFINITY])],
    };
    let result = HnswSnapshot::try_from(raw);
    assert!(
        matches!(result, Err(SnapshotError::NonFiniteVector { .. })),
        "TryFrom must reject embedded vector containing Infinity"
    );
}

/// Verify that valid embedded vectors round-trip through serde without error.
/// NaN cannot be encoded in standard JSON; the TryFrom path is the serde boundary
/// for non-finite values, covered by the try_from_rejects_* tests above.
#[test]
fn deserialize_valid_embedded_vectors_roundtrip() {
    let id1 = make_id(1);
    let valid_snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id1],
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![])]],
        vectors: vec![(id1, vec![0.5_f32, 0.5_f32])],
    };
    let json_str = serde_json::to_string(&valid_snap).expect("serialize");
    let restored: HnswSnapshot = serde_json::from_str(&json_str).expect("deserialize valid");
    assert!(restored.verify().is_ok());
    assert_eq!(restored.vectors.len(), 1);
}

#[test]
fn canonicalize_is_idempotent() {
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);
    let mut snap = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 2,
        tombstone_count: 1,
        max_layer: 1,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id3, id1, id2], // Unsorted
        tombstoned_ids: vec![id3],
        layers: vec![
            vec![(id3, vec![id1]), (id1, vec![id2]), (id2, vec![id3])],
            vec![(id2, vec![]), (id1, vec![])],
        ],

        vectors: vec![],
    };

    snap.canonicalize();
    let after_first = snap.clone();

    snap.canonicalize();
    let after_second = snap.clone();

    // Both passes should produce identical results
    assert_eq!(after_first.indexed_ids, after_second.indexed_ids);
    assert_eq!(after_first.tombstoned_ids, after_second.tombstoned_ids);
    assert_eq!(after_first.layers.len(), after_second.layers.len());
    for (l1, l2) in after_first.layers.iter().zip(after_second.layers.iter()) {
        assert_eq!(l1, l2);
    }
}

#[test]
fn canonical_snapshot_serializes_deterministically() {
    // Create two snapshots with same data but different initial order
    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);

    let mut snap1 = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 3,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id3, id1, id2],
        tombstoned_ids: vec![],
        layers: vec![vec![(id3, vec![id1]), (id1, vec![id2]), (id2, vec![id3])]],

        vectors: vec![],
    };

    let mut snap2 = HnswSnapshot {
        vector_count: 0,
        total_nodes: 3,
        live_nodes: 3,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: sample_config(),
        indexed_ids: vec![id2, id3, id1], // Different initial order
        tombstoned_ids: vec![],
        layers: vec![vec![(id2, vec![id3]), (id3, vec![id1]), (id1, vec![id2])]],

        vectors: vec![],
    };

    snap1.canonicalize();
    snap2.canonicalize();

    let json1 = serde_json::to_string(&snap1).expect("serialize");
    let json2 = serde_json::to_string(&snap2).expect("serialize");

    assert_eq!(
        json1, json2,
        "Canonical snapshots should serialize identically"
    );
}

// ── #415 / #416 regression tests ─────────────────────────────────────────

/// Regression for #415: a legacy (v1-style) snapshot whose `tombstoned_ids`
/// somehow exceeds `indexed_ids` must not panic on unchecked subtraction
/// while normalizing cardinality in `TryFrom<RawHnswSnapshot>`.
#[test]
fn try_from_rejects_legacy_tombstones_exceed_indexed_without_panic() {
    use super::snapshot::RawHnswSnapshot;
    let x = make_id(1);
    let raw = RawHnswSnapshot {
        vector_count: 0,
        total_nodes: 0, // triggers legacy normalization fallback
        live_nodes: 0,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(x),
        config: sample_config(),
        indexed_ids: vec![x],
        tombstoned_ids: vec![x, x], // more tombstones than indexed ids
        layers: vec![vec![(x, vec![])]],
        vectors: vec![],
    };

    let result = HnswSnapshot::try_from(raw);
    assert!(
        result.is_err(),
        "TryFrom must reject invalid legacy cardinality instead of panicking"
    );
}

/// Regression for #415: `verify()` must not panic on unchecked addition
/// when `live_nodes + tombstone_count` would overflow `usize`.
#[test]
fn verify_rejects_wrapping_count_sum_without_panic() {
    let mut snap = sample_snapshot();
    snap.total_nodes = 0;
    snap.live_nodes = usize::MAX;
    snap.tombstone_count = 1;
    snap.indexed_ids = vec![];
    snap.tombstoned_ids = vec![];

    let result = snap.verify();
    assert!(
        matches!(result, Err(SnapshotError::InconsistentCounts { .. })),
        "verify() must reject a wrapping count sum instead of panicking, got {result:?}"
    );
}

/// Regression for #416: `verify()` must reject duplicate `indexed_ids`
/// before restore can build ID maps from them (last-wins insertion would
/// otherwise silently corrupt `internal_to_id`).
#[test]
fn verify_rejects_duplicate_indexed_ids() {
    let x = make_id(1);
    let mut snap = sample_snapshot();
    snap.total_nodes = 2;
    snap.live_nodes = 2;
    snap.tombstone_count = 0;
    snap.indexed_ids = vec![x, x];

    let result = snap.verify();
    assert!(
        matches!(result, Err(SnapshotError::DuplicateIndexedId { id }) if id == x),
        "verify() must reject duplicate indexed_ids, got {result:?}"
    );
}

/// Regression for #416: `verify()` must reject duplicate `tombstoned_ids`
/// before restore can build ID maps from them (last-wins insertion would
/// otherwise silently corrupt the tombstone set).
#[test]
fn verify_rejects_duplicate_tombstoned_ids() {
    let x = make_id(1);
    let mut snap = sample_snapshot();
    snap.total_nodes = 2;
    snap.live_nodes = 0;
    snap.tombstone_count = 2;
    snap.tombstoned_ids = vec![x, x];

    let result = snap.verify();
    assert!(
        matches!(result, Err(SnapshotError::DuplicateTombstonedId { id }) if id == x),
        "verify() must reject duplicate tombstoned_ids, got {result:?}"
    );
}
