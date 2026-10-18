//! Fresh-map L1 + L1.5 ingestion; no allocator instrumentation in this binary.
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_pack_code::source_ingest::run_code_ingest;
use khive_runtime::Namespace;

#[path = "support/source.rs"]
mod source;
use source::{executor, MapFixture, SourceTree, SIZES};

fn sample(rt: &tokio::runtime::Runtime, tree: &SourceTree) -> Duration {
    let fixture = rt.block_on(async { MapFixture::fresh() });
    assert_eq!(rt.block_on(fixture.counts()), [0, 0, 0]);
    let token = fixture
        .runtime
        .authorize(Namespace::local())
        .expect("local token");
    let options = tree.options();
    let start = Instant::now();
    let result = rt.block_on(run_code_ingest(
        &fixture.runtime,
        &token,
        black_box(options),
    ));
    let elapsed = start.elapsed();
    let counts = rt.block_on(fixture.counts());
    drop(token);
    rt.block_on(fixture.close());
    tree.check(&result.expect("complete ingest"));
    assert_eq!(counts, [1, tree.files as i64, tree.files as i64]);
    elapsed
}

fn ingest(c: &mut Criterion) {
    let rt = executor();
    let mut group = c.benchmark_group("pack_code_ingest");
    group.sample_size(10);
    for count in SIZES {
        let tree = SourceTree::new(count);
        let elapsed = sample(&rt, &tree);
        println!(
            "{}",
            serde_json::json!({"scenario":"pack_code_ingest/fresh_l1_l15", "source_files":count,
            "manifest_files":1,"wall_ns":elapsed.as_nanos(),"files_per_second":count as f64 / elapsed.as_secs_f64()})
        );
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("fresh_l1_l15", count), &tree, |b, tree| {
            b.iter_custom(|iterations| {
                (0..iterations).map(|_| sample(&rt, tree)).sum::<Duration>()
            });
        });
    }
    group.finish();
}

criterion_group!(benches, ingest);
criterion_main!(benches);
