use super::*;

pub(super) fn corpus(n: usize, dim: usize) -> Vec<f32> {
    let mut values: Vec<f32> = (0..n * dim)
        .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
        .collect();
    for row in values.chunks_exact_mut(dim) {
        let norm = row.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in row {
            *value /= norm;
        }
    }
    values
}

pub(super) fn config(dim: usize) -> VamanaConfig {
    VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(24)
}

pub(super) fn result_bits(results: Vec<(u32, f32)>) -> Vec<(u32, u32)> {
    results
        .into_iter()
        .map(|(ordinal, distance)| (ordinal, distance.to_bits()))
        .collect()
}

pub(super) fn fresh_search(index: &VamanaIndex, query: &[f32], k: usize) -> Vec<(u32, u32)> {
    let mut visited = VisitedSet::new(index.num_vectors);
    let tombstones = (index.tombstone_count > 0).then_some(index.tombstones.as_slice());
    let result = if index.gs_codec.is_in_distribution(query) {
        let encoded = index.gs_codec.encode(query);
        greedy_search_inner_sq8(
            index.vectors().unwrap(),
            index.dimensions,
            index.gs_codes.view(),
            &index.gs_codec,
            index.graph.adjacency(),
            query,
            &encoded.codes,
            index.graph.medoid(),
            k,
            index.config.search_list_size,
            &mut visited,
            tombstones,
        )
    } else {
        greedy_search_inner(
            index.vectors().unwrap(),
            index.dimensions,
            index.graph.adjacency(),
            query,
            index.graph.medoid(),
            k,
            index.config.search_list_size,
            &mut visited,
            tombstones,
        )
    };
    let mut output = result.results;
    output.sort_unstable_by(|(a_id, a_distance), (b_id, b_distance)| {
        a_distance.total_cmp(b_distance).then(a_id.cmp(b_id))
    });
    result_bits(output)
}

#[test]
fn pooled_search_matches_fresh_visited_results() {
    let vectors = corpus(128, 8);
    let mut index = VamanaIndex::build(&vectors, config(8)).unwrap();
    index.tombstone(9).unwrap();
    let queries = [
        vectors[..8].to_vec(),
        vectors[48..56].to_vec(),
        vec![10.0; 8],
    ];
    for _ in 0..8 {
        for query in &queries {
            for k in [1, 7, 24] {
                let actual = result_bits(index.search(query, k).unwrap());
                assert!(!actual.is_empty());
                assert_eq!(actual, fresh_search(&index, query, k));
                assert!(actual.iter().all(|(ordinal, _)| *ordinal != 9));
            }
        }
    }
}

#[cfg(feature = "mmap")]
#[test]
fn v2_load_still_refuses_corrupted_vector_byte() {
    let dir = tempfile::tempdir().unwrap();
    let index = VamanaIndex::build(&corpus(32, 8), config(8)).unwrap();
    index.save_atomic(dir.path()).unwrap();
    let path = dir.path().join("vectors.bin");
    let mut bytes = fs::read(&path).unwrap();
    bytes[17] ^= 1;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(
        VamanaIndex::load(dir.path()),
        Err(VamanaError::InvalidFormat { ref reason }) if reason.contains("checksum")
    ));
}
