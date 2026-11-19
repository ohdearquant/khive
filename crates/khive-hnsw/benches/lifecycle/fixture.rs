use khive_hnsw::{HnswConfig, HnswIndex, NodeId, RebuildStats};
use rand::{rngs::StdRng, RngExt, SeedableRng};

pub const ROWS: usize = 5_000;
pub const DELETED: usize = 500;
pub const DIMS: usize = 384;
const SEED: u64 = 42;

pub fn node_id(i: usize) -> NodeId {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
    NodeId::new(bytes)
}

pub fn build() -> HnswIndex {
    let config = HnswConfig {
        seed: Some(SEED),
        ..HnswConfig::with_dimensions(DIMS)
    };
    let mut index = HnswIndex::with_config(config);
    let mut rng = StdRng::seed_from_u64(SEED);
    for row in 0..ROWS {
        let mut vector: Vec<f32> = (0..DIMS).map(|_| rng.random_range(-1.0..1.0)).collect();
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!(norm > 0.0);
        for value in &mut vector {
            *value /= norm;
        }
        index.insert(node_id(row), vector).unwrap();
    }
    assert_eq!(index.len(), ROWS);
    assert_eq!(index.len_live(), ROWS);
    assert!(!index.is_quantized());
    index
}

pub fn tombstoned(base: &HnswIndex) -> HnswIndex {
    let mut index = base.clone();
    for row in 0..DELETED {
        assert!(index.delete(node_id(row)));
    }
    assert_eq!(index.len(), ROWS);
    assert_eq!(index.len_live(), ROWS - DELETED);
    assert_eq!(index.tombstone_stats().tombstone_count, DELETED);
    index
}

pub fn assert_compacted(index: &HnswIndex, stats: RebuildStats) {
    assert_eq!(stats.nodes_before, ROWS);
    assert_eq!(stats.nodes_removed, DELETED);
    assert_eq!(stats.nodes_after, ROWS - DELETED);
    assert_eq!(index.len(), ROWS - DELETED);
    assert_eq!(index.len_live(), ROWS - DELETED);
    assert_eq!(index.tombstone_stats().tombstone_count, 0);
    for row in [0, DELETED - 1] {
        assert!(index.get_vector(&node_id(row)).is_none());
    }
    for row in [DELETED, ROWS - 1] {
        assert_eq!(index.get_vector(&node_id(row)).unwrap().len(), DIMS);
    }
}
