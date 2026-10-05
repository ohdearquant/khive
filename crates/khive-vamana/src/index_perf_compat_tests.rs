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

#[test]
fn allocating_search_matches_reference_sq8_fallback_tombstones_and_ties() {
    let mut vectors = corpus(16, 8);
    let first = vectors[..8].to_vec();
    vectors[8..16].copy_from_slice(&first);
    let mut index = VamanaIndex::build(&vectors, config(8).with_max_degree(16)).unwrap();
    index.tombstone(9).unwrap();
    for (ordinal, neighbors) in index.graph.adjacency_mut_for_load().iter_mut().enumerate() {
        *neighbors = (0..16).filter(|node| *node != ordinal as u32).collect();
    }
    index.graph.rebuild_reverse_adj_from_adjacency();
    let outside = vec![10.0; 8];
    assert!(index.gs_codec.is_in_distribution(&first));
    assert!(!index.gs_codec.is_in_distribution(&outside));
    for query in [&first, &outside] {
        for k in [1, 7, 24] {
            let expected = fresh_search(&index, query, k);
            for _ in 0..3 {
                let actual = result_bits(index.search_allocating(query, k).unwrap());
                assert_eq!(actual, expected);
                assert_eq!(actual, result_bits(index.search(query, k).unwrap()));
                assert!(!actual.is_empty());
                assert!(actual.iter().all(|(id, _)| *id != 9));
            }
        }
    }
    let tied = index.search_allocating(&first, 24).unwrap();
    assert_eq!((tied[0].0, tied[1].0), (0, 1));
    assert_eq!(tied[0].1.to_bits(), tied[1].1.to_bits());
}

#[test]
fn allocating_search_does_not_use_or_populate_the_visited_pool() {
    use std::sync::atomic::Ordering;

    let vectors = corpus(32, 8);
    let index = VamanaIndex::build(&vectors, config(8)).unwrap();
    let query = &vectors[..8];
    let expected = fresh_search(&index, query, 7);
    for _ in 0..8 {
        assert_eq!(
            result_bits(index.search_allocating(query, 7).unwrap()),
            expected
        );
        assert!(index.search_visited.idle_nodes().is_empty());
        assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 0);
    }
    assert_eq!(result_bits(index.search(query, 7).unwrap()), expected);
    assert_eq!(index.search_visited.idle_nodes(), vec![32]);
    let mut held = index.search_visited.checkout(32);
    held.clear();
    assert!(held.mark_if_new(7));
    assert_eq!(
        result_bits(index.search_allocating(query, 7).unwrap()),
        expected
    );
    assert!(!held.mark_if_new(7));
    assert!(index.search_visited.idle_nodes().is_empty());
    assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 1);
    drop(held);
    for _ in 0..8 {
        assert_eq!(
            result_bits(index.search_allocating(query, 7).unwrap()),
            expected
        );
        assert_eq!(index.search_visited.idle_nodes(), vec![32]);
        assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 1);
    }
    assert_eq!(result_bits(index.search(query, 7).unwrap()), expected);
    assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 1);
}
