use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use uuid::Uuid;

use khive_fold::{
    cmp_desc_score_then_id, fold_fn, CommonFold, CountFold, Fold, FoldContext, GreedySelector,
    MaxScoreObjective, Objective, ObjectiveContext, ScoredEntry, Selector, SelectorInput,
    SelectorWeights,
};

// ── helpers ──────────────────────────────────────────────────────────────────

/// LCG state for deterministic pseudo-random f32 scores in [0.0, 1.0).
fn lcg_next(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 32) as f32 / u32::MAX as f32
}

fn make_selector_inputs(n: usize) -> Vec<SelectorInput<()>> {
    let mut state = 0xdead_cafe_u64;
    (0..n)
        .map(|i| SelectorInput {
            id: format!("item-{i:06}"),
            content: (),
            size: i % 50 + 1,
            score: lcg_next(&mut state),
            category: Some(format!("cat-{}", i % 8)),
            information_gain: None,
            rank_score: None,
        })
        .collect()
}

fn make_f64_candidates(n: usize) -> Vec<f64> {
    let mut state = 0xcafe_dead_u64;
    (0..n).map(|_| lcg_next(&mut state) as f64).collect()
}

fn make_scored_entries(n: usize) -> Vec<ScoredEntry<Uuid>> {
    let mut state = 0xf00d_cafe_u64;
    (0..n)
        .map(|i| {
            let id = Uuid::from_u128(i as u128);
            let score = lcg_next(&mut state) as f64;
            ScoredEntry::new(id, score, i)
        })
        .collect()
}

// ── fold benchmarks ───────────────────────────────────────────────────────────

fn bench_fold_derive(c: &mut Criterion) {
    let mut g = c.benchmark_group("fold/derive");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let entries: Vec<i64> = (0..n as i64).collect();
        let ctx = FoldContext::new();
        let fold = CommonFold::<i64>::count();

        g.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, e| {
            b.iter(|| fold.derive(black_box(e.iter()), black_box(&ctx)))
        });
    }

    g.finish();
}

fn bench_fold_sum_i64(c: &mut Criterion) {
    let mut g = c.benchmark_group("fold/sum_i64");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let entries: Vec<i64> = (0..n as i64).collect();
        let ctx = FoldContext::new();
        let fold = CommonFold::<i64>::sum_i64(|v: &i64| *v);

        g.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, e| {
            b.iter(|| fold.derive(black_box(e.iter()), black_box(&ctx)))
        });
    }

    g.finish();
}

fn bench_fold_fn_closure(c: &mut Criterion) {
    let mut g = c.benchmark_group("fold/fn_closure");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let entries: Vec<i64> = (0..n as i64).collect();
        let ctx = FoldContext::new();
        let fold = fold_fn(|_ctx| 0i64, |sum: i64, entry: &i64, _ctx| sum + entry);

        g.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, e| {
            b.iter(|| fold.derive(black_box(e.iter()), black_box(&ctx)))
        });
    }

    g.finish();
}

fn bench_count_fold(c: &mut Criterion) {
    let mut g = c.benchmark_group("fold/count");
    g.sample_size(200);

    for n in [10usize, 100, 500] {
        let entries: Vec<i64> = (0..n as i64).collect();
        let ctx = FoldContext::new();
        let fold = CountFold::<i64>::new();

        g.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, e| {
            b.iter(|| fold.derive(black_box(e.iter()), black_box(&ctx)))
        });
    }

    g.finish();
}

// ── objective benchmarks ──────────────────────────────────────────────────────

fn bench_objective_score_batch(c: &mut Criterion) {
    let mut g = c.benchmark_group("objective/batch_score");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let candidates = make_f64_candidates(n);
        let ctx = ObjectiveContext::new();
        let obj = MaxScoreObjective::new(|v: &f64| *v);

        g.bench_with_input(BenchmarkId::from_parameter(n), &candidates, |b, c_| {
            b.iter(|| obj.batch_score(black_box(c_), black_box(&ctx)))
        });
    }

    g.finish();
}

fn bench_objective_select_top(c: &mut Criterion) {
    let mut g = c.benchmark_group("objective/select_top");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let candidates = make_f64_candidates(n);
        let ctx = ObjectiveContext::new();
        let obj = MaxScoreObjective::new(|v: &f64| *v);
        let top_n = (n / 5).max(1);

        g.bench_with_input(BenchmarkId::from_parameter(n), &candidates, |b, c_| {
            b.iter(|| obj.select_top(black_box(c_), black_box(top_n), black_box(&ctx)))
        });
    }

    g.finish();
}

fn bench_objective_select_all(c: &mut Criterion) {
    let mut g = c.benchmark_group("objective/select_all");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let candidates = make_f64_candidates(n);
        let ctx = ObjectiveContext::new();
        let obj = MaxScoreObjective::new(|v: &f64| *v);

        g.bench_with_input(BenchmarkId::from_parameter(n), &candidates, |b, c_| {
            b.iter(|| obj.select(black_box(c_), black_box(&ctx)))
        });
    }

    g.finish();
}

// ── ordering benchmarks ───────────────────────────────────────────────────────

fn bench_ordering_sort(c: &mut Criterion) {
    let mut g = c.benchmark_group("ordering/sort");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let entries = make_scored_entries(n);

        g.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, e| {
            b.iter_batched(
                || e.clone(),
                |mut v| {
                    v.sort_unstable_by(|a, b| b.cmp(a));
                    black_box(v)
                },
                BatchSize::SmallInput,
            )
        });
    }

    g.finish();
}

fn bench_ordering_cmp_desc(c: &mut Criterion) {
    let mut g = c.benchmark_group("ordering/cmp_desc");
    g.sample_size(200);

    let id_a = Uuid::from_u128(1);
    let id_b = Uuid::from_u128(2);

    g.bench_function("scalar", |b| {
        b.iter(|| {
            cmp_desc_score_then_id(
                black_box(0.7),
                black_box(id_a),
                black_box(0.3),
                black_box(id_b),
            )
        })
    });

    g.finish();
}

fn bench_ordering_heap_top_k(c: &mut Criterion) {
    use std::collections::BinaryHeap;

    let mut g = c.benchmark_group("ordering/heap_top_k");
    g.sample_size(100);

    for n in [100usize, 500] {
        let entries = make_scored_entries(n);
        let k = n / 10;

        g.bench_with_input(BenchmarkId::from_parameter(n), &entries, |b, e| {
            b.iter(|| {
                let mut heap: BinaryHeap<ScoredEntry<Uuid>> = e.iter().cloned().collect();
                let top: Vec<_> = (0..k).filter_map(|_| heap.pop()).collect();
                black_box(top)
            })
        });
    }

    g.finish();
}

// ── selector benchmarks ───────────────────────────────────────────────────────

fn bench_selector_greedy(c: &mut Criterion) {
    let mut g = c.benchmark_group("selector/greedy");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let inputs_base = make_selector_inputs(n);
        let budget = n * 25;
        let weights = SelectorWeights::default();

        g.bench_with_input(BenchmarkId::from_parameter(n), &inputs_base, |b, base| {
            b.iter_batched(
                || base.clone(),
                |inputs| {
                    let out = GreedySelector.select(inputs, black_box(budget), black_box(&weights));
                    black_box(out)
                },
                BatchSize::SmallInput,
            )
        });
    }

    g.finish();
}

fn bench_selector_greedy_diversity(c: &mut Criterion) {
    let mut g = c.benchmark_group("selector/greedy_diversity");
    g.sample_size(100);

    for n in [10usize, 100, 500] {
        let inputs_base = make_selector_inputs(n);
        let budget = n * 25;
        let weights = SelectorWeights {
            diversity_bias: 0.5,
            ..Default::default()
        };

        g.bench_with_input(BenchmarkId::from_parameter(n), &inputs_base, |b, base| {
            b.iter_batched(
                || base.clone(),
                |inputs| {
                    let out = GreedySelector.select(inputs, black_box(budget), black_box(&weights));
                    black_box(out)
                },
                BatchSize::SmallInput,
            )
        });
    }

    g.finish();
}

criterion_group!(
    benches,
    bench_fold_derive,
    bench_fold_sum_i64,
    bench_fold_fn_closure,
    bench_count_fold,
    bench_objective_score_batch,
    bench_objective_select_top,
    bench_objective_select_all,
    bench_ordering_sort,
    bench_ordering_cmp_desc,
    bench_ordering_heap_top_k,
    bench_selector_greedy,
    bench_selector_greedy_diversity,
);
criterion_main!(benches);
