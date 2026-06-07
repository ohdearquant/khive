use super::*;
use khive_fold::{Checkpoint, CheckpointStore, FoldContext, InMemoryCheckpointStore};
use uuid::Uuid;

fn make_id(seed: u8) -> NodeId {
    NodeId::new([seed; 16])
}

fn sample_snapshot() -> HnswSnapshot {
    HnswSnapshot {
        vector_count: 0,
        total_nodes: 1,
        live_nodes: 1,
        tombstone_count: 0,
        max_layer: 0,
        entry_point: Some(make_id(1)),
        config: HnswCheckpointConfig {
            m: 16,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![make_id(1)],
        tombstoned_ids: vec![],
        layers: vec![vec![(make_id(1), vec![])]],
        vectors: vec![],
    }
}

fn sample_snapshot_with_tombstones() -> HnswSnapshot {
    let id1 = make_id(1);
    let id2 = make_id(2);
    HnswSnapshot {
        vector_count: 0,
        total_nodes: 2,
        live_nodes: 1,
        tombstone_count: 1,
        max_layer: 0,
        entry_point: Some(id1),
        config: HnswCheckpointConfig {
            m: 16,
            ef_construction: 200,
            metric: "cosine".to_string(),
        },
        indexed_ids: vec![id1, id2],
        tombstoned_ids: vec![id2],
        layers: vec![vec![(id1, vec![id2]), (id2, vec![id1])]],
        vectors: vec![],
    }
}

#[test]
fn create_hnsw_checkpoint() {
    let snap = sample_snapshot();
    let checkpoint: HnswCheckpoint = Checkpoint::new(
        "hnsw_test:ckpt-1",
        snap,
        Uuid::new_v4(),
        100,
        FoldContext::new(),
        1,
    )
    .expect("Checkpoint::new");

    assert_eq!(checkpoint.state.total_nodes, 1);
    assert_eq!(checkpoint.state.live_nodes, 1);
    assert_eq!(checkpoint.entries_processed, 100);
    assert_eq!(checkpoint.fold_version, 1);
}

#[test]
fn create_hnsw_checkpoint_with_tombstones() {
    let snap = sample_snapshot_with_tombstones();
    let checkpoint: HnswCheckpoint = Checkpoint::new(
        "hnsw_test:ckpt-1",
        snap,
        Uuid::new_v4(),
        100,
        FoldContext::new(),
        1,
    )
    .expect("Checkpoint::new");

    assert_eq!(checkpoint.state.total_nodes, 2);
    assert_eq!(checkpoint.state.live_nodes, 1);
    assert_eq!(checkpoint.state.tombstone_count, 1);
    assert_eq!(checkpoint.state.tombstoned_ids.len(), 1);
}

#[test]
fn store_and_load_hnsw_checkpoint() {
    let store: HnswCheckpointStore = InMemoryCheckpointStore::new();
    let snap = sample_snapshot();

    let checkpoint: HnswCheckpoint = Checkpoint::new(
        "hnsw_idx:ckpt-1",
        snap,
        Uuid::new_v4(),
        50,
        FoldContext::new(),
        1,
    )
    .expect("Checkpoint::new");

    store.save(checkpoint).expect("save");

    let loaded = store
        .load("hnsw_idx:ckpt-1")
        .expect("load")
        .expect("should exist");

    assert_eq!(loaded.state.total_nodes, 1);
    assert_eq!(loaded.state.live_nodes, 1);
    assert_eq!(loaded.state.config.m, 16);
    assert_eq!(loaded.state.config.metric, "cosine");
    assert_eq!(loaded.entries_processed, 50);
}

#[test]
fn store_and_load_checkpoint_with_tombstones() {
    let store: HnswCheckpointStore = InMemoryCheckpointStore::new();
    let snap = sample_snapshot_with_tombstones();

    let checkpoint: HnswCheckpoint = Checkpoint::new(
        "hnsw_idx:ckpt-tomb",
        snap,
        Uuid::new_v4(),
        50,
        FoldContext::new(),
        1,
    )
    .expect("Checkpoint::new");

    store.save(checkpoint).expect("save");

    let loaded = store
        .load("hnsw_idx:ckpt-tomb")
        .expect("load")
        .expect("should exist");

    assert_eq!(loaded.state.total_nodes, 2);
    assert_eq!(loaded.state.live_nodes, 1);
    assert_eq!(loaded.state.tombstone_count, 1);
    assert!(loaded.state.verify().is_ok());
}

#[test]
fn load_latest_hnsw_checkpoint() {
    let store: HnswCheckpointStore = InMemoryCheckpointStore::new();

    for i in 0..3 {
        let mut snap = sample_snapshot();
        snap.total_nodes = (i + 1) * 100;
        snap.live_nodes = (i + 1) * 100;

        let checkpoint: HnswCheckpoint = Checkpoint::new(
            format!("hnsw_idx:ckpt-{i}"),
            snap,
            Uuid::new_v4(),
            (i + 1) * 10,
            FoldContext::new(),
            1,
        )
        .expect("Checkpoint::new");
        store.save(checkpoint).expect("save");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let latest = store
        .load_latest("hnsw_idx")
        .expect("load_latest")
        .expect("should exist");

    assert_eq!(latest.state.total_nodes, 300);
    assert_eq!(latest.state.live_nodes, 300);
    assert_eq!(latest.entries_processed, 30);
}
