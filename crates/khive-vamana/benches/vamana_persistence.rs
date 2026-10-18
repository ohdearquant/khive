//! File-backed directory checkpoint round-trips; requires the mmap feature.

mod support;

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};
use khive_vamana::VamanaIndex;
use support::{build, unit_vectors, verify_clean_round_trip, BASE_ROWS, BASE_SEED};

fn persistence(c: &mut Criterion) {
    let mut base = None;
    let mut group = c.benchmark_group("filesystem_round_trip");
    for atomic in [false, true] {
        let name = if atomic {
            "save_atomic_load_5000"
        } else {
            "save_load_5000"
        };
        group.bench_function(name, |b| {
            let base = base.get_or_insert_with(|| build(BASE_ROWS, BASE_SEED));
            let query = unit_vectors(1, BASE_SEED + 2);
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    // These APIs persist a DIRECTORY of segments, not one file. Setup is untimed.
                    let dir = tempfile::tempdir().expect("checkpoint directory");
                    let start = Instant::now();
                    if atomic {
                        base.save_atomic(dir.path()).expect("v2 atomic save");
                    } else {
                        base.save(dir.path()).expect("v1 save");
                    }
                    let restored = VamanaIndex::load(dir.path()).expect("checkpoint load");
                    elapsed += start.elapsed();
                    verify_clean_round_trip(base, &restored, &query);
                    drop(black_box(restored)); // Release mappings before deleting their files.
                    dir.close().expect("remove checkpoint directory");
                }
                elapsed
            });
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10).warm_up_time(Duration::from_millis(100)).measurement_time(Duration::from_secs(1));
    targets = persistence
}
criterion_main!(benches);
