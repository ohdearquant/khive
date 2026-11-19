use super::perf_compat_tests::{config, corpus, fresh_search, result_bits};
use super::*;
use std::sync::{atomic::Ordering, Barrier};

#[test]
fn idle_capacity_bounds_overlapping_search_waves() {
    let pool = SearchVisitedPool::default();
    for wave in 0..3 {
        let leases: Vec<_> = (0..10).map(|_| pool.checkout(128)).collect();
        assert!(pool.idle_nodes().is_empty());
        assert_eq!(pool.allocations.load(Ordering::Relaxed), 10 + wave * 2);
        drop(leases);
        assert_eq!(pool.idle_nodes(), vec![128; 8]);
    }
    let warmed = pool.allocations.load(Ordering::Relaxed);
    for _ in 0..16 {
        drop(pool.checkout(128));
    }
    assert_eq!(pool.allocations.load(Ordering::Relaxed), warmed);
}

#[test]
fn full_idle_pool_does_not_alias_active_concurrent_leases() {
    let pool = SearchVisitedPool::default();
    let barrier = Barrier::new(3);
    let (idle, independent) = std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                let mut lease = pool.checkout(64);
                let first_visit = lease.mark_if_new(7);
                barrier.wait();
                barrier.wait();
                assert!(first_visit);
                assert!(!lease.mark_if_new(7));
                lease.clear();
                assert!(lease.mark_if_new(7));
            });
        }
        barrier.wait();
        let leases: Vec<_> = (0..8).map(|_| pool.checkout(64)).collect();
        drop(leases);
        let idle = pool.idle_nodes();
        let mut cached = pool.checkout(64);
        let independent = cached.mark_if_new(7);
        drop(cached);
        barrier.wait();
        (idle, independent)
    });
    assert_eq!(idle, vec![64; 8]);
    assert!(independent);
    assert_eq!(pool.idle_nodes(), vec![64; 8]);
    assert_eq!(pool.allocations.load(Ordering::Relaxed), 10);
}

#[test]
fn vector_count_changes_replace_scratch_and_reset_marks() {
    let pool = SearchVisitedPool::default();
    {
        let mut lease = pool.checkout(128);
        assert!(lease.mark_if_new(7));
    }
    for (step, nodes) in [129, 16, 0, 16].into_iter().enumerate() {
        let mut lease = pool.checkout(nodes);
        assert!(!lease.is_marked(7));
        if nodes > 7 {
            assert!(lease.mark_if_new(7));
        }
        drop(lease);
        assert_eq!(pool.idle_nodes(), vec![nodes]);
        assert_eq!(pool.allocations.load(Ordering::Relaxed), step + 2);
    }
}

#[test]
fn consolidation_releases_large_scratch_and_preserves_noop_reuse() {
    let vectors = corpus(64, 8);
    let mut index = VamanaIndex::build(&vectors, config(8)).unwrap();
    let leases: Vec<_> = (0..8).map(|_| index.search_visited.checkout(64)).collect();
    drop(leases);
    assert!(index.consolidate().unwrap().is_empty());
    assert_eq!(index.search_visited.idle_nodes(), vec![64; 8]);
    for ordinal in 8..64 {
        index.tombstone(ordinal).unwrap();
    }
    assert_eq!(index.consolidate().unwrap().len(), 8);
    assert!(index.search_visited.idle_nodes().is_empty());
    let warmed = index.search_visited.allocations.load(Ordering::Relaxed);
    let query = &vectors[..8];
    for _ in 0..8 {
        assert_eq!(
            result_bits(index.search(query, 7).unwrap()),
            fresh_search(&index, query, 7)
        );
        assert_eq!(index.search_visited.idle_nodes(), vec![8]);
    }
    assert_eq!(
        index.search_visited.allocations.load(Ordering::Relaxed),
        warmed + 1
    );
}

#[test]
fn maintenance_fork_has_independent_idle_scratch() {
    let vectors = corpus(32, 8);
    let original = VamanaIndex::build(&vectors, config(8)).unwrap();
    let query = &vectors[..8];
    let expected = result_bits(original.search(query, 7).unwrap());
    let mut fork = original.fork_for_maintenance();
    assert!(fork.search_visited.idle_nodes().is_empty());
    assert_eq!(original.search_visited.idle_nodes(), vec![32]);
    assert_eq!(result_bits(fork.search(query, 7).unwrap()), expected);
    fork.tombstone(9).unwrap();
    fork.consolidate().unwrap();
    assert!(fork.search_visited.idle_nodes().is_empty());
    assert_eq!(original.search_visited.idle_nodes(), vec![32]);
    assert_eq!(result_bits(original.search(query, 7).unwrap()), expected);
    assert_eq!(
        original.search_visited.allocations.load(Ordering::Relaxed),
        1
    );
}

#[test]
fn append_replaces_scratch_while_recycled_ordinals_reuse_it() {
    let vectors = corpus(32, 8);
    let mut index = VamanaIndex::build(&vectors, config(8)).unwrap();
    let query = &vectors[..8];
    index.search(query, 7).unwrap();
    assert_eq!(index.search_visited.idle_nodes(), vec![32]);
    assert_eq!(index.insert(query).unwrap(), 32);
    assert_eq!(
        result_bits(index.search(query, 7).unwrap()),
        fresh_search(&index, query, 7)
    );
    assert_eq!(index.search_visited.idle_nodes(), vec![33]);
    assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 2);
    index.tombstone(9).unwrap();
    assert_eq!(index.insert(query).unwrap(), 9);
    for _ in 0..8 {
        assert_eq!(
            result_bits(index.search(query, 7).unwrap()),
            fresh_search(&index, query, 7)
        );
    }
    assert_eq!(index.search_visited.idle_nodes(), vec![33]);
    assert_eq!(index.search_visited.allocations.load(Ordering::Relaxed), 2);
}

#[test]
fn bounded_pool_preserves_sq8_f32_tombstone_and_tie_result_bits() {
    let mut vectors = corpus(16, 8);
    let duplicated = vectors[..8].to_vec();
    vectors[8..16].copy_from_slice(&duplicated);
    let mut index = VamanaIndex::build(&vectors, config(8).with_max_degree(16)).unwrap();
    index.tombstone(9).unwrap();
    // A stale adjacency can still name a deleted node; both kernels must filter it.
    for (ordinal, neighbors) in index.graph.adjacency_mut_for_load().iter_mut().enumerate() {
        *neighbors = (0..16).filter(|node| *node != ordinal as u32).collect();
    }
    index.graph.rebuild_reverse_adj_from_adjacency();
    let in_distribution = &vectors[..8];
    let out_of_distribution = vec![10.0; 8];
    assert!(index.gs_codec.is_in_distribution(in_distribution));
    assert!(!index.gs_codec.is_in_distribution(&out_of_distribution));
    let leases: Vec<_> = (0..10).map(|_| index.search_visited.checkout(16)).collect();
    drop(leases);
    for _ in 0..8 {
        for query in [in_distribution, out_of_distribution.as_slice()] {
            let expected = fresh_search(&index, query, 24);
            let actual = result_bits(index.search(query, 24).unwrap());
            assert_eq!(actual, expected);
            assert!(!actual.is_empty());
            assert!(actual.iter().all(|(ordinal, _)| *ordinal != 9));
        }
        let tied = index.search(in_distribution, 24).unwrap();
        assert_eq!(tied[0].0, 0);
        assert_eq!(tied[1].0, 1);
        assert_eq!(tied[0].1.to_bits(), tied[1].1.to_bits());
    }
    assert_eq!(index.search_visited.idle_nodes(), vec![16; 8]);
}
