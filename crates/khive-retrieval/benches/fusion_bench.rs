use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use khive_fusion::FusionStrategy;
use khive_retrieval::{
    eval::{compute_all, LabeledResult, RetrievalLabel},
    filter_by_policy, filter_by_predicate,
    hybrid::fuse_search_results,
    ClearanceLevel, HybridConfig, SearchConfig, SearchPolicy,
};
use khive_score::DeterministicScore;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Data generation helpers
// ---------------------------------------------------------------------------

/// Generate a ranked result list of `n` items with deterministic scores
/// seeded by `source_id` to differentiate vector vs keyword sources.
fn make_hits(n: usize, source_id: u64) -> Vec<(String, DeterministicScore)> {
    (0..n)
        .map(|i| {
            // Interleave rankings so some IDs overlap between sources
            let doc_id = (i * 3 + source_id as usize * 7) % (n * 2);
            let score = 1.0 - (i as f64 / n as f64);
            (
                format!("doc_{doc_id:04}"),
                DeterministicScore::from_f64(score),
            )
        })
        .collect()
}

/// Generate a labeled result slice with a fixed pattern for eval benchmarks.
fn make_labeled_results(n: usize) -> Vec<LabeledResult> {
    let labels = [
        RetrievalLabel::Decisive,
        RetrievalLabel::Supporting,
        RetrievalLabel::Background,
        RetrievalLabel::Irrelevant,
        RetrievalLabel::AdjacentWrong,
    ];
    (0..n)
        .map(|i| LabeledResult {
            section_id: Uuid::from_u64_pair(0, i as u64),
            score: 1.0 - (i as f64 / n as f64),
            label: labels[i % labels.len()],
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Group 1: fuse_search_results — strategy × input-size matrix
// ---------------------------------------------------------------------------

fn bench_fuse_rrf(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse/rrf");
    group.sample_size(200);

    for n in [50usize, 100, 250, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let source1 = make_hits(n, 0);
            let source2 = make_hits(n, 1);
            let config = HybridConfig::new(10)
                .with_pool_size(n)
                .with_fusion_strategy(FusionStrategy::Rrf { k: 60 });
            // Use iter_batched so per-iteration Vec cloning is excluded from the timed loop;
            // each call to fuse_search_results takes owned Vecs so we must clone per iteration.
            b.iter_batched(
                || vec![source1.clone(), source2.clone()],
                |sources| fuse_search_results(black_box(sources), black_box(&config)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_fuse_weighted(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse/weighted");
    group.sample_size(200);

    for n in [50usize, 100, 250, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let source1 = make_hits(n, 0);
            let source2 = make_hits(n, 1);
            let config = HybridConfig::new(10)
                .with_pool_size(n)
                .with_fusion_strategy(FusionStrategy::weighted(vec![0.7, 0.3]));
            b.iter_batched(
                || vec![source1.clone(), source2.clone()],
                |sources| fuse_search_results(black_box(sources), black_box(&config)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_fuse_union(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse/union");
    group.sample_size(200);

    for n in [50usize, 100, 250, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let source1 = make_hits(n, 0);
            let source2 = make_hits(n, 1);
            let config = HybridConfig::new(10)
                .with_pool_size(n)
                .with_fusion_strategy(FusionStrategy::Union);
            b.iter_batched(
                || vec![source1.clone(), source2.clone()],
                |sources| fuse_search_results(black_box(sources), black_box(&config)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Group 2: fuse with 3 sources (vector + keyword + graph simulation)
// ---------------------------------------------------------------------------

fn bench_fuse_three_sources(c: &mut Criterion) {
    let mut group = c.benchmark_group("fuse/three_sources");
    group.sample_size(100);

    for n in [50usize, 200, 500] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let s1 = make_hits(n, 0);
            let s2 = make_hits(n, 1);
            let s3 = make_hits(n, 2);
            let config = HybridConfig::new(10)
                .with_pool_size(n)
                .with_fusion_strategy(FusionStrategy::Rrf { k: 60 });
            b.iter_batched(
                || vec![s1.clone(), s2.clone(), s3.clone()],
                |sources| fuse_search_results(black_box(sources), black_box(&config)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Group 3: HybridConfig construction and builder chains
// ---------------------------------------------------------------------------

fn bench_hybrid_config_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("hybrid_config");
    group.sample_size(200);

    group.bench_function("new", |b| b.iter(|| HybridConfig::new(black_box(10))));

    group.bench_function("builder_rrf", |b| {
        b.iter(|| {
            HybridConfig::new(black_box(10))
                .with_fusion_strategy(black_box(FusionStrategy::Rrf { k: 60 }))
                .with_pool_size(black_box(100))
                .with_weights(black_box(0.7), black_box(0.3))
        })
    });

    group.bench_function("builder_weighted", |b| {
        b.iter(|| {
            HybridConfig::new(black_box(10))
                .with_fusion_strategy(black_box(FusionStrategy::weighted(vec![0.6, 0.4])))
                .with_pool_size(black_box(50))
        })
    });

    group.bench_function("normalized_weights", |b| {
        let config = HybridConfig::default();
        b.iter(|| black_box(config.normalized_weights()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 4: SearchConfig construction and presets
// ---------------------------------------------------------------------------

fn bench_search_config_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_config");
    group.sample_size(200);

    group.bench_function("default", |b| b.iter(|| black_box(SearchConfig::default())));

    group.bench_function("vector_only", |b| {
        b.iter(|| black_box(SearchConfig::vector_only()))
    });

    group.bench_function("keyword_only", |b| {
        b.iter(|| black_box(SearchConfig::keyword_only()))
    });

    group.bench_function("hybrid_balanced", |b| {
        b.iter(|| black_box(SearchConfig::hybrid_balanced()))
    });

    group.bench_function("candidate_pool_size", |b| {
        let cfg = SearchConfig::default();
        b.iter(|| black_box(cfg.candidate_pool_size()))
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Group 5: Policy filtering
// ---------------------------------------------------------------------------

fn bench_policy_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("policy_filter");
    group.sample_size(200);

    for n in [50usize, 200, 500] {
        let results: Vec<(String, DeterministicScore)> = (0..n)
            .map(|i| {
                (
                    format!("doc_{i}"),
                    DeterministicScore::from_f64(1.0 - i as f64 / n as f64),
                )
            })
            .collect();

        group.bench_with_input(
            BenchmarkId::new("filter_by_policy_public", n),
            &n,
            |b, _| {
                let policy = SearchPolicy::public();
                b.iter(|| {
                    filter_by_policy(black_box(results.clone()), black_box(&policy), |_id| {
                        ClearanceLevel::Public
                    })
                })
            },
        );

        group.bench_with_input(BenchmarkId::new("filter_by_policy_mixed", n), &n, |b, _| {
            let policy = SearchPolicy::internal();
            b.iter(|| {
                filter_by_policy(black_box(results.clone()), black_box(&policy), |id| {
                    // Half the docs are secret, half are public
                    let idx: usize = id.trim_start_matches("doc_").parse().unwrap_or(0);
                    if idx.is_multiple_of(2) {
                        ClearanceLevel::Secret
                    } else {
                        ClearanceLevel::Public
                    }
                })
            })
        });

        group.bench_with_input(BenchmarkId::new("filter_by_predicate", n), &n, |b, _| {
            b.iter(|| {
                filter_by_predicate(black_box(results.clone()), |id| {
                    id.ends_with('0') || id.ends_with('2')
                })
            })
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Group 6: Eval metrics (compute_all, recall_at_k, ndcg_at_k, mrr)
// ---------------------------------------------------------------------------

fn bench_eval_metrics(c: &mut Criterion) {
    let mut group = c.benchmark_group("eval_metrics");
    group.sample_size(200);

    for n in [10usize, 50, 100, 500] {
        let results = make_labeled_results(n);

        group.bench_with_input(BenchmarkId::new("compute_all", n), &n, |b, _| {
            b.iter(|| compute_all(black_box(&results)))
        });

        group.bench_with_input(BenchmarkId::new("recall_at_k_5", n), &n, |b, _| {
            b.iter(|| khive_retrieval::eval::recall_at_k(black_box(&results), black_box(5)))
        });

        group.bench_with_input(BenchmarkId::new("ndcg_at_k_10", n), &n, |b, _| {
            b.iter(|| khive_retrieval::eval::ndcg_at_k(black_box(&results), black_box(10)))
        });

        group.bench_with_input(BenchmarkId::new("mrr", n), &n, |b, _| {
            b.iter(|| khive_retrieval::eval::mrr(black_box(&results)))
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Group 7: RetrievalLabel gain scoring (hot loop in eval)
// ---------------------------------------------------------------------------

fn bench_label_scoring(c: &mut Criterion) {
    let mut group = c.benchmark_group("label_scoring");
    group.sample_size(200);

    let all_labels = [
        RetrievalLabel::Decisive,
        RetrievalLabel::Supporting,
        RetrievalLabel::Background,
        RetrievalLabel::Irrelevant,
        RetrievalLabel::AdjacentWrong,
    ];

    group.bench_function("gain_all_variants", |b| {
        b.iter(|| {
            let mut sum = 0.0_f64;
            for &label in &all_labels {
                sum += black_box(label).gain();
            }
            black_box(sum)
        })
    });

    group.bench_function("is_relevant_all_variants", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for &label in &all_labels {
                if black_box(label).is_relevant() {
                    count += 1;
                }
            }
            black_box(count)
        })
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion wiring
// ---------------------------------------------------------------------------

criterion_group!(
    fusion_benches,
    bench_fuse_rrf,
    bench_fuse_weighted,
    bench_fuse_union,
    bench_fuse_three_sources,
);

criterion_group!(
    config_benches,
    bench_hybrid_config_construction,
    bench_search_config_construction,
);

criterion_group!(policy_benches, bench_policy_filter);

criterion_group!(eval_benches, bench_eval_metrics, bench_label_scoring);

criterion_main!(fusion_benches, config_benches, policy_benches, eval_benches);
