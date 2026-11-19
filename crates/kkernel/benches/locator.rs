//! Bounded locator operations and capacity hit census.

mod routing_support;

use std::collections::BTreeSet;
use std::hint::black_box;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use khive_runtime::BackendId;
use kkernel::coordinator::LocatorCache;
use routing_support::{group, id, scene, Workload};

const LIVE_TTL: Duration = Duration::from_secs(86_400);
const EXPECTED_DEFAULT_CAPACITY: usize = 65_536;

#[derive(Clone, Copy)]
enum Operation {
    Get,
    Insert,
    PurgeLive,
}

struct CacheCase {
    cache: LocatorCache,
    backend: BackendId,
    count: usize,
    next: u64,
    operation: Operation,
}

fn fill(cache: &LocatorCache, count: usize, backend: &BackendId) {
    for index in 1..=count as u64 {
        cache.insert(id(index), backend.clone());
    }
}

fn hit_census(count: usize) {
    let backend = BackendId::main();
    let cache = LocatorCache::with_ttl(LIVE_TTL);
    fill(&cache, count, &backend);
    let retained = count.min(EXPECTED_DEFAULT_CAPACITY);
    assert_eq!(cache.len(), retained);
    let hits = (1..=count as u64)
        .filter(|&index| cache.get(id(index)).as_ref() == Some(&backend))
        .count();
    assert_eq!(hits, retained);
    println!(
        "locator_capacity_report capacity=default requested={count} retained={retained} hits={hits} hit_rate={}",
        hits as f64 / count as f64
    );
}

impl CacheCase {
    fn new(count: usize, operation: Operation) -> Self {
        let backend = BackendId::main();
        let cache = LocatorCache::with_ttl_and_capacity(
            LIVE_TTL,
            NonZeroUsize::new(count).expect("positive fixture capacity"),
        );
        fill(&cache, count, &backend);
        assert_eq!(cache.len(), count);
        let hits = (1..=count as u64)
            .filter(|&index| cache.get(id(index)).as_ref() == Some(&backend))
            .count();
        assert_eq!(hits, count);
        println!(
            "locator_capacity_report capacity={count} requested={count} retained={count} hits={hits} hit_rate=1"
        );
        Self {
            cache,
            backend,
            count,
            next: count as u64 + 1,
            operation,
        }
    }
}

impl Workload for CacheCase {
    fn sample(&mut self) -> Duration {
        let index = self.next;
        self.next = self.next.checked_add(1).expect("bounded UUID sequence");
        match self.operation {
            Operation::Get => {
                let key = id(index % self.count as u64 + 1);
                let started = Instant::now();
                let found = black_box(self.cache.get(black_box(key)));
                let elapsed = started.elapsed();
                assert_eq!(found.as_ref(), Some(&self.backend));
                elapsed
            }
            Operation::Insert => {
                let key = id(index);
                let backend = self.backend.clone();
                assert!(self.cache.get(key).is_none(), "insert must be fresh");
                let started = Instant::now();
                self.cache.insert(black_box(key), black_box(backend));
                let elapsed = started.elapsed();
                assert_eq!(self.cache.len(), self.count, "capacity remains saturated");
                assert_eq!(self.cache.get(key).as_ref(), Some(&self.backend));
                elapsed
            }
            Operation::PurgeLive => {
                assert_eq!(self.cache.get(id(1)).as_ref(), Some(&self.backend));
                let started = Instant::now();
                black_box(&self.cache).purge_expired();
                let elapsed = started.elapsed();
                assert_eq!(self.cache.len(), self.count);
                assert_eq!(self.cache.get(id(1)).as_ref(), Some(&self.backend));
                elapsed
            }
        }
    }
}

fn locator(c: &mut Criterion) {
    let mut group = group(c, "locator");
    let mut census_sizes = BTreeSet::new();
    for count in [1_000, 10_000, 100_000] {
        for (name, operation) in [
            ("get_hit", Operation::Get),
            ("insert_new_lru", Operation::Insert),
            ("purge_live", Operation::PurgeLive),
        ] {
            scene(&mut group, &format!("{name}/{count}"), || {
                if census_sizes.insert(count) {
                    hit_census(count);
                }
                CacheCase::new(count, operation)
            });
        }
    }
    group.finish();
}

criterion_group!(benches, locator);
criterion_main!(benches);
