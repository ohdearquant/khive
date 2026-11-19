use khive_vamana::{VamanaConfig, VamanaIndex};
use rand::{rngs::StdRng, RngExt, SeedableRng};

pub const DIM: usize = 384;
pub const BASE_ROWS: usize = 5000;
pub const BASE_SEED: u64 = 42;

pub fn unit_vectors(rows: usize, seed: u64) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut vectors: Vec<f32> = (0..rows * DIM)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    for row in vectors.chunks_exact_mut(DIM) {
        let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm.is_finite() && norm > 0.0, "invalid generated vector");
        for value in row {
            *value /= norm;
        }
    }
    vectors
}

pub fn build(rows: usize, seed: u64) -> VamanaIndex {
    // Default R64/L128 matches the existing ordinary recall quality fixture.
    let index = VamanaIndex::build(
        &unit_vectors(rows, seed),
        VamanaConfig::with_dimensions(DIM),
    )
    .expect("generated index build");
    assert_eq!(index.num_vectors(), rows);
    assert_eq!(index.live_count(), rows);
    assert_eq!(index.tombstone_count(), 0);
    index
}

pub fn verify_clean_round_trip(original: &VamanaIndex, restored: &VamanaIndex, query: &[f32]) {
    assert_eq!(restored.num_vectors(), original.num_vectors());
    assert_eq!(restored.dimensions(), DIM);
    assert_eq!(restored.live_count(), original.live_count());
    assert_eq!(restored.tombstone_count(), 0);
    assert_eq!(restored.config(), original.config());
    assert_eq!(
        restored.vectors().expect("restored vectors"),
        original.vectors().expect("original vectors")
    );
    assert_eq!(
        restored.search(query, 10).expect("restored search"),
        original.search(query, 10).expect("original search")
    );
}
