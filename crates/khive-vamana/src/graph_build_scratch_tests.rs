//! Allocation accounting for Vamana's visited-set construction factory on dedicated workers.

use super::*;

struct PassLimit;

impl PassLimit {
    fn new(passes: usize) -> Self {
        set_max_passes(Some(passes));
        Self
    }
}

impl Drop for PassLimit {
    fn drop(&mut self) {
        set_max_passes(None);
    }
}

fn allocation_count() -> usize {
    VISITED_SET_ALLOCATIONS.with(|count| count.get())
}

fn fixture_vectors() -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(0x3787);
    (0..(BUILD_BATCH_SIZE + 17) * 4)
        .map(|_| rng.gen_range(-1.0..1.0))
        .collect()
}

fn check_allocations<F: Fn() -> Result<VamanaGraph> + Send + Sync>(build: F) {
    for passes in [1, 2] {
        #[cfg(feature = "parallel")]
        for workers in [1, 2, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap();
            let mut before = pool.broadcast(|worker| (worker.index(), allocation_count()));
            before.sort_unstable_by_key(|(worker, _)| *worker);
            pool.install(|| {
                let _limit = PassLimit::new(passes);
                build().unwrap();
            });
            let mut after = pool.broadcast(|worker| (worker.index(), allocation_count()));
            after.sort_unstable_by_key(|(worker, _)| *worker);
            assert_eq!(before.len(), workers);
            assert_eq!(after.len(), workers);
            let mut total = 0;
            for ((before_worker, before_count), (after_worker, after_count)) in
                before.into_iter().zip(after)
            {
                assert_eq!(before_worker, after_worker);
                let allocations = after_count - before_count;
                assert!(
                    allocations <= passes,
                    "worker {after_worker} allocated {allocations} visited sets for {passes} passes"
                );
                if after_worker == 0 {
                    assert_eq!(
                        allocations, passes,
                        "the first worker processes every nonempty batch"
                    );
                }
                total += allocations;
            }
            assert!(
                total > 0,
                "the counter must observe actual tracker allocations"
            );
            assert!(total <= workers * passes);
        }
        #[cfg(not(feature = "parallel"))]
        {
            let before = allocation_count();
            let _limit = PassLimit::new(passes);
            build().unwrap();
            assert_eq!(allocation_count() - before, passes);
        }
    }
}

#[test]
fn build_allocates_at_most_one_visited_set_per_worker_per_pass() {
    let vectors = fixture_vectors();
    let config = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    check_allocations(|| VamanaGraph::build(&vectors, &config));
}

#[test]
fn build_sq8_allocates_at_most_one_visited_set_per_worker_per_pass() {
    let vectors = fixture_vectors();
    let config = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let codec = GsSq8Codec::train_flat(&vectors, 4);
    let encoded = codec.encode_flat_par(&vectors, 4);
    check_allocations(|| {
        VamanaGraph::build_sq8(&vectors, CodesView::Owned(&encoded), &codec, &config)
    });
}
