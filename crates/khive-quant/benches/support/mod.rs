use rand::{rngs::StdRng, RngExt, SeedableRng};

pub const ROWS: usize = 1000;
pub const SEED: u64 = 0xA052_BE42;

/// Generated row-major unit vectors; fixtures are never loaded from disk.
pub fn corpus(dims: usize) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(SEED ^ dims as u64);
    let mut values: Vec<f32> = (0..ROWS * dims)
        .map(|_| rng.random_range(-1.0f32..1.0))
        .collect();
    for row in values.chunks_exact_mut(dims) {
        let norm = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm > 0.0 && norm.is_finite());
        for x in row {
            *x /= norm;
        }
    }
    values
}
