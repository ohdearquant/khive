use khive_hnsw::NodeId;
use khive_hnsw::{HnswConfig, HnswIndex};
use std::collections::HashMap;

fn make_id(seed: u8) -> NodeId {
    NodeId::new([seed; 16])
}

fn generate_random_vector(dim: usize, seed: u64) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    (0..dim)
        .map(|i| {
            let mut hasher = DefaultHasher::new();
            (seed, i).hash(&mut hasher);
            (hasher.finish() as f32 / u64::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

/// Snapshot from a populated index includes vector data.
#[test]
fn snapshot_includes_vector_data() {
    let mut index = HnswIndex::new(4);

    let id1 = make_id(1);
    let id2 = make_id(2);
    let vec1 = vec![1.0, 0.0, 0.0, 0.0];
    let vec2 = vec![0.0, 1.0, 0.0, 0.0];

    index.insert(id1, vec1.clone()).expect("insert");
    index.insert(id2, vec2.clone()).expect("insert");

    let snap = index.snapshot();

    assert_eq!(snap.vectors.len(), 2, "snapshot should contain 2 vectors");

    // Vectors are sorted by NodeId bytes — look up by id
    let vec_map: HashMap<NodeId, &Vec<f32>> = snap.vectors.iter().map(|(id, v)| (*id, v)).collect();

    assert_eq!(
        vec_map.get(&id1).copied(),
        Some(&vec1),
        "id1 vector should match"
    );
    assert_eq!(
        vec_map.get(&id2).copied(),
        Some(&vec2),
        "id2 vector should match"
    );
}

/// Empty index snapshot has no vectors.
#[test]
fn snapshot_empty_index_has_no_vectors() {
    let index = HnswIndex::new(4);
    let snap = index.snapshot();
    assert!(
        snap.vectors.is_empty(),
        "empty index snapshot has no vectors"
    );
}

/// Self-contained round-trip: snapshot() → restore_from_snapshot_embedded().
#[test]
fn snapshot_restore_embedded_round_trip() {
    let config = HnswConfig::with_dimensions(8).with_seed(42);
    let mut original = HnswIndex::with_config(config.clone());

    let ids: Vec<NodeId> = (0..20u8).map(make_id).collect();
    let vecs: Vec<Vec<f32>> = (0..20u64).map(|i| generate_random_vector(8, i)).collect();

    for (id, vec) in ids.iter().zip(vecs.iter()) {
        original.insert(*id, vec.clone()).expect("insert");
    }

    // Take a snapshot and restore into a fresh index
    let snap = original.snapshot();
    assert_eq!(
        snap.vectors.len(),
        20,
        "snapshot should embed all 20 vectors"
    );

    let mut restored = HnswIndex::with_config(config);
    restored
        .restore_from_snapshot_embedded(&snap)
        .expect("restore embedded");

    assert_eq!(restored.len(), 20, "restored index should have 20 nodes");

    // Search results should match the original
    let query = generate_random_vector(8, 999);
    let results_orig = original.search(&query, 5).expect("search original");
    let results_rest = restored.search(&query, 5).expect("search restored");

    assert_eq!(
        results_orig.len(),
        results_rest.len(),
        "result count should match"
    );
    for (r_orig, r_rest) in results_orig.iter().zip(results_rest.iter()) {
        assert_eq!(r_orig.0, r_rest.0, "result IDs should match");
    }
}

/// Tombstoned nodes are preserved through embedded snapshot round-trip.
#[test]
fn snapshot_restore_embedded_preserves_tombstones() {
    let config = HnswConfig::with_dimensions(4).with_seed(42);
    let mut index = HnswIndex::with_config(config.clone());

    let id1 = make_id(1);
    let id2 = make_id(2);
    let id3 = make_id(3);

    index.insert(id1, vec![1.0, 0.0, 0.0, 0.0]).expect("insert");
    index.insert(id2, vec![0.0, 1.0, 0.0, 0.0]).expect("insert");
    index.insert(id3, vec![0.0, 0.0, 1.0, 0.0]).expect("insert");

    // Tombstone id2
    assert!(index.delete(id2));

    let snap = index.snapshot();
    assert_eq!(snap.total_nodes, 3);
    assert_eq!(snap.tombstone_count, 1);
    assert_eq!(snap.vectors.len(), 3, "all 3 vectors including tombstone");

    let mut restored = HnswIndex::with_config(config);
    restored
        .restore_from_snapshot_embedded(&snap)
        .expect("restore");

    assert_eq!(restored.len(), 3, "total nodes preserved");
    assert_eq!(
        restored.tombstone_stats().tombstone_count,
        1,
        "tombstone count preserved"
    );

    // Search should not return id2
    let results = restored.search(&[0.0, 1.0, 0.0, 0.0], 3).expect("search");
    let result_ids: Vec<NodeId> = results.iter().map(|(id, _)| *id).collect();
    assert!(
        !result_ids.contains(&id2),
        "tombstoned id2 should not appear in results"
    );
}

/// Snapshot serialization includes vectors and round-trips via JSON.
#[test]
fn snapshot_serialization_includes_vectors() {
    let mut index = HnswIndex::new(4);

    let id1 = make_id(1);
    index.insert(id1, vec![1.0, 2.0, 3.0, 4.0]).expect("insert");

    let snap = index.snapshot();
    assert!(!snap.vectors.is_empty(), "vectors should be in snapshot");

    let json = serde_json::to_string(&snap).expect("serialize");
    assert!(
        json.contains("vectors"),
        "serialized JSON should contain vectors field"
    );

    let restored_snap: khive_hnsw::checkpoint::HnswSnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        restored_snap.vectors.len(),
        1,
        "deserialized snapshot should have 1 vector"
    );
    assert_eq!(
        restored_snap.vectors[0].0, id1,
        "vector id should be preserved"
    );
    assert_eq!(
        restored_snap.vectors[0].1,
        vec![1.0, 2.0, 3.0, 4.0],
        "vector data should be preserved"
    );
}

/// Old snapshots (no vectors field) still deserialize correctly.
#[test]
fn backward_compat_snapshot_without_vectors() {
    // Simulate a snapshot from before the vectors field was added
    let old_json = r#"{
            "total_nodes": 1,
            "live_nodes": 1,
            "tombstone_count": 0,
            "max_layer": 0,
            "entry_point": "01010101010101010101010101010101",
            "config": {"m": 16, "ef_construction": 200, "metric": "cosine"},
            "indexed_ids": ["01010101010101010101010101010101"],
            "tombstoned_ids": [],
            "layers": []
        }"#;

    let snap: khive_hnsw::checkpoint::HnswSnapshot =
        serde_json::from_str(old_json).expect("deserialize old snapshot");

    assert_eq!(snap.total_nodes, 1);
    assert!(
        snap.vectors.is_empty(),
        "old snapshot deserialized with empty vectors"
    );
    assert!(snap.verify().is_ok(), "old snapshot should verify");
}

/// restore_from_snapshot_embedded fails when snapshot has no vectors.
#[test]
fn restore_embedded_fails_without_vectors() {
    let config = HnswConfig::with_dimensions(4);
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    let snap = khive_hnsw::checkpoint::HnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: khive_hnsw::checkpoint::HnswCheckpointConfig {
            m: 20,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1],
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![])]],
        vectors: vec![], // Intentionally empty
    };

    let result = index.restore_from_snapshot_embedded(&snap);
    assert!(
        result.is_err(),
        "should fail when snapshot has no embedded vectors"
    );
}

/// restore_from_snapshot with external map takes priority over embedded vectors.
#[test]
fn restore_external_overrides_embedded_vectors() {
    let config = HnswConfig::with_dimensions(4).with_seed(42);
    let mut source = HnswIndex::with_config(config.clone());

    let id1 = make_id(1);
    source
        .insert(id1, vec![1.0, 0.0, 0.0, 0.0])
        .expect("insert");

    let snap = source.snapshot();

    // Provide a different vector for id1 via the external map
    let updated_vector = vec![0.0, 0.0, 0.0, 1.0];
    let external: HashMap<NodeId, Vec<f32>> = [(id1, updated_vector.clone())].into_iter().collect();

    let mut restored = HnswIndex::with_config(config);
    restored
        .restore_from_snapshot(&snap, &external)
        .expect("restore with external");

    // The restored index should use the external (override) vector
    let retrieved = restored.get_vector(&id1).expect("get vector");
    assert_eq!(
        retrieved, updated_vector,
        "external vector should override embedded"
    );
}

/// restore_from_snapshot rejects snapshot with entry_point not in indexed_ids.
#[test]
fn restore_rejects_entry_point_not_in_indexed_ids() {
    let config = HnswConfig::with_dimensions(4);
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    let id2 = make_id(2); // not in indexed_ids

    let snap = khive_hnsw::checkpoint::HnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id2), // not in indexed_ids!
        config: khive_hnsw::checkpoint::HnswCheckpointConfig {
            m: 20,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1],
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![])]],
        vectors: vec![(id1, vec![1.0, 0.0, 0.0, 0.0])],
    };

    let vectors = HashMap::new();
    let result = index.restore_from_snapshot(&snap, &vectors);
    assert!(
        result.is_err(),
        "should reject entry_point not in indexed_ids"
    );
}

/// restore_from_snapshot rejects vectors with wrong dimensions BEFORE clearing.
#[test]
fn restore_rejects_wrong_dimensions_before_clearing() {
    let config = HnswConfig::with_dimensions(4);
    let mut original = HnswIndex::with_config(config.clone());

    let id_orig = make_id(99);
    original
        .insert(id_orig, vec![1.0, 0.0, 0.0, 0.0])
        .expect("insert");

    let id1 = make_id(1);
    // Snapshot with wrong-dimension vector
    let snap = khive_hnsw::checkpoint::HnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(id1),
        config: khive_hnsw::checkpoint::HnswCheckpointConfig {
            m: 20,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1],
        tombstoned_ids: vec![],
        layers: vec![vec![(id1, vec![])]],
        vectors: vec![(id1, vec![1.0, 0.0])], // wrong dim: 2 instead of 4
    };

    let vectors = HashMap::new();
    let result = original.restore_from_snapshot(&snap, &vectors);
    assert!(result.is_err(), "should reject wrong dimensions");

    // Original index must be unmodified
    assert_eq!(
        original.len(),
        1,
        "index should be unmodified after failed restore"
    );
    assert!(
        original.get_vector(&id_orig).is_some(),
        "original node must still be present"
    );
}

/// Regression for #416: restore must reject a snapshot with duplicate
/// `indexed_ids` before mutating the existing index. Accepting it would
/// let last-wins `HashMap::insert` corrupt `internal_to_id` with two
/// live internal nodes mapped to the same external ID.
#[test]
fn restore_rejects_duplicate_indexed_ids_before_mutation() {
    let config = HnswConfig::with_dimensions(4);
    let mut index = HnswIndex::with_config(config.clone());

    let keep_id = make_id(99);
    index
        .insert(keep_id, vec![1.0, 0.0, 0.0, 0.0])
        .expect("insert");

    let x = make_id(1);
    let snap = khive_hnsw::checkpoint::HnswSnapshot {
        vector_count: 0,
        total_nodes: 2,
        live_nodes: 2,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(x),
        config: khive_hnsw::checkpoint::HnswCheckpointConfig {
            m: 20,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![x, x], // duplicate
        tombstoned_ids: vec![],
        layers: vec![vec![(x, vec![])]],
        vectors: vec![(x, vec![0.0, 1.0, 0.0, 0.0])],
    };

    let vectors = HashMap::new();
    let result = index.restore_from_snapshot(&snap, &vectors);
    assert!(result.is_err(), "should reject duplicate indexed_ids");

    assert_eq!(
        index.len(),
        1,
        "existing index must be unmodified after rejected restore"
    );
    assert!(
        index.get_vector(&keep_id).is_some(),
        "existing node must still be present"
    );
}

/// Large snapshot round-trip preserves search quality.
#[test]
fn snapshot_restore_preserves_search_quality() {
    let config = HnswConfig::with_dimensions(32).with_seed(42);
    let mut original = HnswIndex::with_config(config.clone());

    let n = 200usize;
    for i in 0..n {
        let id = NodeId::new({
            let mut b = [0u8; 16];
            b[0] = (i & 0xff) as u8;
            b[1] = (i >> 8) as u8;
            b
        });
        let vec = generate_random_vector(32, i as u64);
        original.insert(id, vec).expect("insert");
    }

    let snap = original.snapshot();
    assert_eq!(snap.vectors.len(), n, "all vectors embedded");

    let mut restored = HnswIndex::with_config(config);
    restored
        .restore_from_snapshot_embedded(&snap)
        .expect("restore");

    // Search quality: >= 80% recall@10 across 10 queries
    let k = 10;
    let mut total_recall = 0.0;
    for q in 0..10 {
        let query = generate_random_vector(32, 10_000 + q);

        let results_orig: std::collections::HashSet<NodeId> = original
            .search(&query, k)
            .expect("orig search")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let results_rest: std::collections::HashSet<NodeId> = restored
            .search(&query, k)
            .expect("rest search")
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let overlap = results_orig.intersection(&results_rest).count();
        total_recall += overlap as f32 / k as f32;
    }

    let avg_recall = total_recall / 10.0;
    assert!(
        avg_recall >= 0.8,
        "restored index recall {avg_recall:.2} should be >= 0.8"
    );
}
