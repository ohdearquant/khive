//! Generated lifecycle and recall evaluation workloads; see docs/benchmarks.md.

mod support;

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_vamana::VamanaIndex;
use support::{build, unit_vectors, verify_clean_round_trip, BASE_ROWS, BASE_SEED, DIM};

fn deletion_ids(index: &VamanaIndex, count: usize) -> Vec<u32> {
    let ids: Vec<_> = (0..index.num_vectors() as u32)
        .filter(|&id| id != index.graph().medoid())
        .take(count)
        .collect();
    assert_eq!(ids.len(), count);
    ids
}

fn mutations(c: &mut Criterion) {
    // Lazy setup avoids building a graph when no mutation scenario was selected.
    let mut base = None;
    c.bench_function("lifecycle_insert/100_into_5000", |b| {
        let base = base.get_or_insert_with(|| build(BASE_ROWS, BASE_SEED));
        let additions = unit_vectors(100, BASE_SEED + 1);
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = base.fork_for_maintenance();
                let mut assigned = Vec::with_capacity(100);
                let start = Instant::now();
                for vector in additions.chunks_exact(DIM) {
                    assigned.push(index.insert(black_box(vector)).expect("insert vector"));
                }
                elapsed += start.elapsed();
                assert_eq!(
                    assigned,
                    (BASE_ROWS as u32..BASE_ROWS as u32 + 100).collect::<Vec<_>>()
                );
                assert_eq!(index.num_vectors(), BASE_ROWS + 100);
                assert_eq!(index.live_count(), BASE_ROWS + 100);
                assert_eq!(index.ops_since_consolidation(), 100);
                black_box(index);
            }
            elapsed
        });
    });
    let mut group = c.benchmark_group("lifecycle_tombstone");
    for batch in [false, true] {
        let name = if batch {
            "batch_100_of_5000"
        } else {
            "single_100_of_5000"
        };
        group.bench_function(name, |b| {
            let base = base.get_or_insert_with(|| build(BASE_ROWS, BASE_SEED));
            let ids = deletion_ids(base, 100);
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let mut index = base.fork_for_maintenance();
                    let start = Instant::now();
                    if batch {
                        index
                            .tombstone_batch(black_box(&ids))
                            .expect("batch tombstone");
                    } else {
                        for &id in &ids {
                            index.tombstone(black_box(id)).expect("tombstone");
                        }
                    }
                    elapsed += start.elapsed();
                    assert_eq!(index.num_vectors(), BASE_ROWS);
                    assert_eq!(index.tombstone_count(), 100);
                    assert_eq!(index.live_count(), BASE_ROWS - 100);
                    assert!(ids.iter().all(|&id| index.is_tombstoned(id)));
                    black_box(index);
                }
                elapsed
            });
        });
    }
    group.finish();
    // Retain the pre-tombstoned base across Criterion callback invocations.
    let mut deleted = None;
    c.bench_function("lifecycle_consolidate/500_tombstones_of_5000", |b| {
        let deleted = deleted.get_or_insert_with(|| {
            // Earlier scenarios are complete: reuse their clean base instead of
            // retaining both clean and deleted baselines plus a working fork.
            let mut index = base.take().unwrap_or_else(|| build(BASE_ROWS, BASE_SEED));
            let ids = deletion_ids(&index, 500);
            index.tombstone_batch(&ids).expect("prepare 500 tombstones");
            assert_eq!(index.tombstone_count(), 500);
            index
        });
        let expected: Vec<_> = (0..BASE_ROWS as u32)
            .filter(|&id| !deleted.is_tombstoned(id))
            .collect();
        assert_eq!(expected.len(), 4500);
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = deleted.fork_for_maintenance();
                let start = Instant::now();
                let remap = index.consolidate().expect("consolidate");
                elapsed += start.elapsed();
                assert_eq!(remap, expected);
                assert_eq!(index.num_vectors(), 4500);
                assert_eq!(index.live_count(), 4500);
                assert_eq!(index.tombstone_count(), 0);
                assert_eq!(index.ops_since_consolidation(), 0);
                black_box(index);
            }
            elapsed
        });
    });
}

fn portable(c: &mut Criterion) {
    let mut base = None;
    c.bench_function("portable_round_trip/5000", |b| {
        let base = base.get_or_insert_with(|| build(BASE_ROWS, BASE_SEED));
        let ids: Vec<_> = (0..BASE_ROWS as u32)
            .map(|i| (i, format!("vector-{i}")))
            .collect();
        let query = unit_vectors(1, BASE_SEED + 2);
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let start = Instant::now();
                let bytes = base.to_bytes(black_box(&ids)).expect("portable encode");
                let (restored, restored_ids) =
                    VamanaIndex::from_bytes(black_box(&bytes)).expect("portable decode");
                elapsed += start.elapsed();
                assert_eq!(restored_ids, ids);
                verify_clean_round_trip(base, &restored, &query);
                black_box((bytes, restored));
            }
            elapsed
        });
    });
}

struct RecallReport {
    recall: f64,
    median: Duration,
    p99: Duration,
    queries: usize,
}

fn recall_report(index: &VamanaIndex, queries: &[f32]) -> RecallReport {
    assert_eq!(queries.len() % DIM, 0);
    let count = queries.len() / DIM;
    assert!(count > 0);
    // Warm the search scratch and reference evaluator before the measured fixed pass.
    index
        .recall_at_k(&queries[..DIM], 10)
        .expect("warm recall evaluation");
    let mut samples = Vec::with_capacity(count);
    let mut recall = 0.0;
    for query in queries.chunks_exact(DIM) {
        let start = Instant::now();
        let value = index
            .recall_at_k(black_box(query), 10)
            .expect("recall evaluation");
        let duration = start.elapsed();
        assert!(value.is_finite() && (0.0..=1.0).contains(&value));
        samples.push(duration);
        recall += value;
    }
    assert_eq!(samples.len(), count);
    samples.sort_unstable();
    let lower = samples[(count - 1) / 2];
    let upper = samples[count / 2];
    RecallReport {
        recall: recall / count as f64,
        median: lower + (upper - lower) / 2,
        p99: samples[(99 * count).div_ceil(100) - 1],
        queries: count,
    }
}

fn recall(c: &mut Criterion) {
    let mut group = c.benchmark_group("recall_at_k");
    for (n, corpus_seed, query_seed) in [
        (1000usize, 0xDEAD_BEEF, 0xCAFE_BABE),
        (5000usize, 0xFEED_FACE, 0xABCD_1234),
    ] {
        let mut fixture = None;
        let mut reported = false;
        let mut position = 0;
        group.bench_function(BenchmarkId::new("per_query_k10", n), |b| {
            let (index, queries) = fixture.get_or_insert_with(||
                (build(n, corpus_seed), unit_vectors(n, query_seed)));
            if !reported {
                let report = recall_report(index, queries);
                assert_eq!(report.queries, n);
                if n == 1000 {
                    assert!(report.recall >= 0.80,
                        "1000-query fixture recall@10 {} is below 0.80", report.recall);
                }
                println!("recall_at_k/per_query_k10/{n}/distribution queries={} recall_at_10={:.6} eval_median_ns={} eval_p99_ns={}",
                    report.queries, report.recall, report.median.as_nanos(), report.p99.as_nanos());
                reported = true;
            }
            // Criterion samples one rotating query. The fixed pass above covers all N queries;
            // its actual per-query distribution is not inferred from Criterion's mean estimate.
            b.iter(|| {
                let query = &queries[position * DIM..(position + 1) * DIM];
                position = (position + 1) % n;
                black_box(index.recall_at_k(black_box(query), 10).expect("recall query"))
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = mutations, portable, recall
}
criterion_main!(benches);
