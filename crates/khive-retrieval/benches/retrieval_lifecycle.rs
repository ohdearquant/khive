//! Public orchestration and materialization; generated fixtures, no model or storage I/O.
use std::cell::OnceCell;
use std::convert::Infallible;
use std::future::{ready, Ready};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use khive_retrieval::{
    fuse_two_stage, materialize_ranked_prefix, DropReason, DualIndexConfig, DualIndexRouter,
    DualIndexStrategy, FusionStrategy, MaterializationDecision, MaterializationLimits,
    MaterializedPrefix, RankedCandidate,
};
use khive_score::DeterministicScore;
use serde_json::Value;

#[path = "fixtures/mode.rs"]
mod mode;

const ROWS: usize = 500;
type Rows = Vec<(usize, DeterministicScore)>;
type Prefix = MaterializedPrefix<usize, usize, usize, Reason>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    Missing,
    Filtered,
}

impl DropReason for Reason {
    const ALL: &'static [Self] = &[Self::Missing, Self::Filtered];

    fn ordinal(self) -> usize {
        match self {
            Self::Missing => 0,
            Self::Filtered => 1,
        }
    }
}

fn batch() -> NonZeroUsize {
    NonZeroUsize::new(64).expect("nonzero batch")
}

fn limits() -> MaterializationLimits {
    MaterializationLimits::try_new(ROWS, batch(), ROWS, 64).expect("valid limits")
}

async fn materialize(candidates: Vec<RankedCandidate<usize, usize>>) -> Prefix {
    materialize_ranked_prefix(
        candidates,
        ROWS,
        batch(),
        limits(),
        |candidate| candidate.key,
        |_| Ok::<(), Infallible>(()),
        |keys| {
            ready(Ok(keys
                .into_iter()
                .filter(|key| !key.is_multiple_of(5))
                .map(|key| (key, key))
                .collect()))
        },
        |_, row| match row {
            None => MaterializationDecision::Drop(Reason::Missing),
            Some(key) if key % 5 == 1 => MaterializationDecision::Drop(Reason::Filtered),
            Some(key) => MaterializationDecision::Keep(key),
        },
    )
    .await
    .expect("materialization")
}

fn check_prefix(result: &Prefix) {
    assert_eq!(result.accepted.len(), 300);
    assert_eq!(result.drop_counts.count(Reason::Missing), Some(100));
    assert_eq!(result.drop_counts.count(Reason::Filtered), Some(100));
    assert_eq!(result.drop_counts.total(), 200);
    assert_eq!(result.diagnostic_details.len(), 64);
    assert!(result.diagnostics_truncated);
    let expected: Vec<_> = (0..ROWS).filter(|key| key % 5 >= 2).collect();
    assert_eq!(
        result
            .accepted
            .iter()
            .map(|item| item.output)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(result
        .accepted
        .iter()
        .enumerate()
        .all(|(i, item)| item.rank == i + 1));
}

fn ranked(start: usize) -> Rows {
    (0..ROWS)
        .map(|rank| {
            (
                start + rank,
                DeterministicScore::from_f64(1.0 / (rank + 1) as f64),
            )
        })
        .collect()
}

fn check_ranking(rows: &Rows, count: usize) {
    assert_eq!(rows.len(), count);
    let unique: std::collections::BTreeSet<_> = rows.iter().map(|(id, _)| *id).collect();
    assert_eq!(unique.len(), count);
    assert!(rows.windows(2).all(|pair| {
        pair[0].1 > pair[1].1 || (pair[0].1 == pair[1].1 && pair[0].0 < pair[1].0)
    }));
}

fn no_custom(
    _: String,
    _: Value,
    _: [Rows; 2],
    _: usize,
) -> Option<Ready<khive_retrieval::Result<Rows>>> {
    None
}

fn lifecycle(c: &mut Criterion) {
    let profile = mode::is_profile_mode();
    let runtime = OnceCell::new();
    let get_runtime = || {
        runtime.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
        })
    };
    let materialization = OnceCell::new();
    let get_materialization = || {
        materialization.get_or_init(|| {
            let candidates: Vec<_> = (0..ROWS)
                .map(|key| RankedCandidate {
                    key,
                    score: ROWS - key,
                })
                .collect();
            let prefix = get_runtime().block_on(materialize(candidates.clone()));
            check_prefix(&prefix);
            (candidates, prefix)
        })
    };
    if profile {
        get_materialization();
    }
    let mut group = c.benchmark_group("materialization");
    group.bench_function("limits_500", |b| {
        b.iter(|| {
            black_box(
                MaterializationLimits::try_new(
                    black_box(ROWS),
                    black_box(batch()),
                    black_box(ROWS),
                    black_box(64),
                )
                .expect("limits"),
            )
        })
    });
    group.bench_function("drop_counts_500", |b| {
        let (_, prefix) = get_materialization();
        b.iter(|| {
            let counts = black_box(&prefix.drop_counts);
            black_box((
                counts.count(Reason::Missing),
                counts.count(Reason::Filtered),
                counts.total(),
            ))
        })
    });
    group.bench_function("prefix_500", |b| {
        let (candidates, _) = get_materialization();
        let rt = get_runtime();
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let input = candidates.clone();
                let start = Instant::now();
                let result = rt.block_on(materialize(black_box(input)));
                elapsed += start.elapsed();
                check_prefix(&result);
                black_box(result);
            }
            elapsed
        })
    });
    group.finish();

    let orchestration = OnceCell::new();
    let get_orchestration = || {
        orchestration.get_or_init(|| {
            let strategy = FusionStrategy::Rrf { k: 60 };
            let router = DualIndexRouter::new(DualIndexConfig::default().with_strategy(
                DualIndexStrategy::Both {
                    fusion: strategy.clone(),
                },
            ));
            (ranked(0), ranked(250), ranked(500), strategy, router)
        })
    };
    if profile {
        get_orchestration();
    }
    let mut group = c.benchmark_group("orchestration");
    group.bench_function("dual_index_2x500", |b| {
        let (primary, legacy, _, _, router) = get_orchestration();
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let (a, b) = (primary.clone(), legacy.clone());
                let start = Instant::now();
                let result = router
                    .merge_results(black_box(a), black_box(b), ROWS)
                    .expect("dual fusion");
                elapsed += start.elapsed();
                check_ranking(&result, ROWS);
                assert!(result.iter().all(|(id, _)| *id < 750));
                black_box(result);
            }
            elapsed
        })
    });
    group.bench_function("two_stage_fusion_2x500_plus_text500", |b| {
        let (primary, legacy, text, strategy, _) = get_orchestration();
        let rt = get_runtime();
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iterations {
                let (arms, text) = (vec![primary.clone(), legacy.clone()], text.clone());
                let start = Instant::now();
                let result = rt
                    .block_on(fuse_two_stage(
                        black_box(arms),
                        Some(strategy),
                        black_box(text),
                        strategy,
                        no_custom,
                    ))
                    .expect("two-stage fusion");
                elapsed += start.elapsed();
                check_ranking(&result, 1_000);
                let mut ids: Vec<_> = result.iter().map(|(id, _)| *id).collect();
                ids.sort_unstable();
                assert_eq!(ids, (0..1_000).collect::<Vec<_>>());
                black_box(result);
            }
            elapsed
        })
    });
    group.finish();
}

criterion_group!(benches, lifecycle);
criterion_main!(benches);
