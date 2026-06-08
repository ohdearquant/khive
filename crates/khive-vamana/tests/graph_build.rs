//! Integration tests for Vamana graph construction — covers valid builds, empty/small cases, and error paths.

use khive_vamana::{VamanaConfig, VamanaError, VamanaGraph};
use rand::{prelude::*, SeedableRng};

#[test]
fn build_rejects_empty_vectors() {
    let cfg = VamanaConfig::default();
    assert!(matches!(
        VamanaGraph::build(&[], &cfg),
        Err(VamanaError::EmptyInput)
    ));
}

#[test]
fn build_rejects_non_row_major_vectors() {
    let cfg = VamanaConfig::with_dimensions(3);
    let vectors = vec![0.1f32; 7]; // 7 not divisible by 3
    assert!(matches!(
        VamanaGraph::build(&vectors, &cfg),
        Err(VamanaError::DimensionMismatch { .. })
    ));
}

#[test]
fn build_creates_bounded_degree_graph() {
    let mut rng = StdRng::seed_from_u64(42);
    let n = 50usize;
    let dim = 8usize;
    let mut raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
    normalize_rows(&mut raw, dim);

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);

    let g = VamanaGraph::build(&raw, &cfg).unwrap();
    for list in g.adjacency() {
        assert!(list.len() <= 8);
    }
}

#[test]
fn build_is_deterministic_for_same_input() {
    let mut rng = StdRng::seed_from_u64(77);
    let n = 30usize;
    let dim = 4usize;
    let mut raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
    normalize_rows(&mut raw, dim);

    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(6)
        .with_search_list_size(12);

    let g1 = VamanaGraph::build(&raw, &cfg).unwrap();
    let g2 = VamanaGraph::build(&raw, &cfg).unwrap();
    assert_eq!(g1, g2);
}

fn normalize_rows(v: &mut [f32], dim: usize) {
    for row in v.chunks_mut(dim) {
        let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in row.iter_mut() {
                *x /= norm;
            }
        }
    }
}
