use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::time::{Duration, Instant};

#[path = "lifecycle/fixture.rs"]
mod fixture;
#[path = "lifecycle/heap.rs"]
mod heap;

#[global_allocator]
static ALLOCATOR: heap::MeasuredAllocator = heap::MeasuredAllocator;

fn bench_rebuild_allocation(c: &mut Criterion) {
    let mut base = None;
    let mut group = c.benchmark_group("hnsw_rebuild_allocation");
    group.sample_size(20);
    let mut reported = false;
    group.bench_function("n5000_tombstones500", |b| {
        let base = base.get_or_insert_with(|| fixture::tombstoned(&fixture::build()));
        if !reported {
            let mut peaks = Vec::with_capacity(32);
            for _ in 0..32 {
                let mut index = base.clone();
                let (stats, peak) = heap::measure_peak(|| index.rebuild());
                fixture::assert_compacted(&index, stats);
                peaks.push(peak);
            }
            peaks.sort_unstable();
            let median = (peaks[15] as f64 + peaks[16] as f64) / 2.0;
            println!(
                "hnsw_rebuild_allocation/n5000_tombstones500 \
                 samples=32 peak_additional_requested_bytes_median={median} \
                 peak_additional_requested_bytes_max={}",
                peaks[31]
            );
            reported = true;
        }
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = base.clone();
                let start = Instant::now();
                let (stats, peak) = heap::measure_peak(|| index.rebuild());
                elapsed += start.elapsed();
                black_box(peak);
                fixture::assert_compacted(&index, stats);
            }
            elapsed
        });
    });
    group.finish();
}

criterion_group!(benches, bench_rebuild_allocation);
criterion_main!(benches);
