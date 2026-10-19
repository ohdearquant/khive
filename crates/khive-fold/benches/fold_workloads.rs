#[path = "workload_support/anchor.rs"]
mod anchor;
#[path = "workload_support/checkpoint.rs"]
mod checkpoint;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_fold::{
    Anchor, BfsAnchor, CountFold, FilterFold, Fold, FoldContext, MapFold, SequentialFold,
    SumI64Fold,
};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("compose");
    group.bench_function("filter_even/500", |b| {
        let entries: Vec<i64> = (0..500).collect();
        let context = FoldContext::new();
        let fold = FilterFold::new(CountFold::<i64>::new(), |entry: &i64| entry % 2 == 0);
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let result = fold.derive(black_box(entries.iter()), &context);
                total += start.elapsed();
                assert_eq!(result.state, 250);
                assert_eq!(result.entries_processed, 500);
            }
            total
        });
    });
    group.bench_function("map_double/500", |b| {
        let entries: Vec<i64> = (0..500).collect();
        let context = FoldContext::new();
        let fold = MapFold::new(SumI64Fold::new(|entry: &i64| *entry), |entry: &i64| {
            entry * 2
        });
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let result = fold.derive(black_box(entries.iter()), &context);
                total += start.elapsed();
                assert_eq!(result.state, 249_500);
                assert_eq!(result.entries_processed, 500);
            }
            total
        });
    });
    group.bench_function("sequential_two_stages/500", |b| {
        let entries: Vec<i64> = (0..500).collect();
        let context = FoldContext::new().with_extra(serde_json::json!({"carry":0}));
        let pair = SequentialFold::new(stage(), stage(), context_after);
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let (first, second) = pair.execute(black_box(entries.iter()), black_box(&context));
                total += start.elapsed();
                assert_eq!(first.state, 124_750);
                assert_eq!(second.state, 249_500);
                assert_eq!(first.entries_processed, 500);
                assert_eq!(second.entries_processed, 500);
            }
            total
        });
    });
    group.bench_function("sequential_composed_five_stages/500", |b| {
        let entries: Vec<i64> = (0..500).collect();
        let context = FoldContext::new().with_extra(serde_json::json!({"carry":0}));
        let pair = SequentialFold::new(stage(), stage(), context_after);
        let terminal = stage();
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let (first, second) = pair.execute(black_box(entries.iter()), black_box(&context));
                let third_context = context_after(&second.state, &context);
                let (third, fourth) = pair.execute(black_box(entries.iter()), &third_context);
                let fifth_context = context_after(&fourth.state, &third_context);
                let fifth = terminal.derive(black_box(entries.iter()), &fifth_context);
                total += start.elapsed();
                for (index, result) in [first, second, third, fourth, fifth]
                    .into_iter()
                    .enumerate()
                {
                    assert_eq!(result.state, (index as i64 + 1) * 124_750);
                    assert_eq!(result.entries_processed, 500);
                }
            }
            total
        });
    });
    group.finish();
    let mut group = c.benchmark_group("checkpoint/save_load");
    for count in [10, 100] {
        group.bench_function(BenchmarkId::from_parameter(count), |b| {
            let fixture = checkpoint::Fixture::new(count);
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    total += fixture.sample();
                }
                total
            });
        });
    }
    group.finish();
    let mut group = c.benchmark_group("anchor");
    for count in [100, 500] {
        for credit in [false, true] {
            group.bench_function(
                BenchmarkId::new(if credit { "credit" } else { "trace" }, count),
                |b| {
                    let fixture = anchor::Fixture::new(count);
                    b.iter_custom(|iters| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            if credit {
                                let start = Instant::now();
                                let result = BfsAnchor.credit(
                                    black_box(&fixture.graph),
                                    &fixture.graph.nodes[count - 1],
                                    count,
                                );
                                total += start.elapsed();
                                fixture.validate_credit(result.expect("complete credit traversal"));
                            } else {
                                let start = Instant::now();
                                let result = BfsAnchor.trace(
                                    black_box(&fixture.graph),
                                    &fixture.graph.nodes[0],
                                    count,
                                );
                                total += start.elapsed();
                                fixture.validate_trace(result.expect("complete forward traversal"));
                            }
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}
criterion_group! { name = workloads; config = Criterion::default().sample_size(10).warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)); targets = benches }
criterion_main!(workloads);

fn stage() -> impl Fold<i64, i64> {
    khive_fold::fold_fn(
        |ctx: &FoldContext| {
            ctx.extra()
                .get("carry")
                .and_then(serde_json::Value::as_i64)
                .expect("explicit previous-stage carry")
        },
        |state, entry: &i64, _| state + entry,
    )
}

fn context_after(state: &i64, context: &FoldContext) -> FoldContext {
    context
        .clone()
        .with_extra(serde_json::json!({"carry":state}))
}
