use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[path = "lifecycle/fixture.rs"]
mod fixture;

use fixture::{assert_compacted, build, node_id, tombstoned, ROWS};

fn bench_lifecycle(c: &mut Criterion) {
    let base = OnceLock::new();
    let mut group = c.benchmark_group("hnsw_lifecycle");
    group.sample_size(20);

    group.bench_function("delete100_n5000", |b| {
        let base = base.get_or_init(build);
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = base.clone();
                let start = Instant::now();
                let removed = (0..100).filter(|&row| index.delete(node_id(row))).count();
                elapsed += start.elapsed();
                assert_eq!(black_box(removed), 100);
                assert_eq!(index.len(), ROWS);
                assert_eq!(index.len_live(), ROWS - 100);
                assert_eq!(index.tombstone_stats().tombstone_count, 100);
            }
            elapsed
        });
    });

    let deleted = OnceLock::new();
    group.bench_function("rebuild_n5000_tombstones500", |b| {
        let deleted = deleted.get_or_init(|| tombstoned(base.get_or_init(build)));
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = deleted.clone();
                let start = Instant::now();
                let stats = black_box(index.rebuild());
                elapsed += start.elapsed();
                assert_compacted(&index, stats);
            }
            elapsed
        });
    });
    drop(deleted);

    group.bench_function("clear_n5000", |b| {
        let base = base.get_or_init(build);
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = base.clone();
                let start = Instant::now();
                black_box(&mut index).clear();
                elapsed += start.elapsed();
                assert!(index.is_empty());
                assert_eq!(index.len_live(), 0);
                assert_eq!(index.tombstone_stats().tombstone_count, 0);
                assert!(index.get_vector(&node_id(0)).is_none());
            }
            elapsed
        });
    });

    let mut quantized = None;
    group.bench_function("set_quantized_true_n5000", |b| {
        let index = quantized.get_or_insert_with(|| base.get_or_init(build).clone());
        index.set_quantized(false);
        b.iter(|| black_box(&mut *index).set_quantized(black_box(true)));
        assert!(index.is_quantized());
        assert_eq!(index.len_live(), ROWS);
    });
    group.bench_function("is_quantized_n5000", |b| {
        let index = quantized.get_or_insert_with(|| base.get_or_init(build).clone());
        index.set_quantized(true);
        b.iter(|| black_box(black_box(&*index).is_quantized()));
    });
    group.finish();
}

criterion_group!(benches, bench_lifecycle);
criterion_main!(benches);
