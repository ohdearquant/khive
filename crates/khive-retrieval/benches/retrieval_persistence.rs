//! Shared-storage snapshot persistence; database creation/migration/cleanup are untimed.
use std::cell::OnceCell;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_retrieval::{RetrievalPersistence, ShadowValidationConfig, ShadowValidationResult};
use khive_storage::types::SqlStatement;
use khive_storage::SqlAccess;

#[path = "fixtures/persistence.rs"]
mod fixture;
#[path = "fixtures/mode.rs"]
mod mode;
use fixture::{Fixture, Indexes, ROWS};

const NAMESPACE: &str = "bench:retrieval:甲'current";
const OTHER: &str = "bench:retrieval:sentinel";

#[derive(Clone, Copy)]
enum Kind {
    Bm25,
    Hnsw,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::Hnsw => "hnsw",
        }
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Fresh,
    Replace,
    Shadow,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::Fresh => "new_save_load_500",
            Self::Replace => "namespace_replace_save_load_500",
            Self::Shadow => "namespace_shadow_500",
        }
    }
}

async fn save(persistence: &RetrievalPersistence, kind: Kind, indexes: &Indexes) {
    match kind {
        Kind::Bm25 => persistence.persist_bm25_index(&indexes.bm25).await,
        Kind::Hnsw => persistence.persist_hnsw_snapshot(&indexes.hnsw).await,
    }
    .expect("persist complete snapshot");
}

// Keep the loaded typed value alive until after timing; JSON equality is untimed.
// Keep results on the stack: boxing solely for enum layout would add measured allocations.
#[allow(clippy::large_enum_variant)]
enum Loaded {
    Bm25(khive_retrieval::Bm25Index),
    Hnsw(khive_retrieval::HnswSnapshot),
}

async fn load(persistence: &RetrievalPersistence, kind: Kind) -> Loaded {
    match kind {
        Kind::Bm25 => Loaded::Bm25(
            persistence
                .load_bm25_index()
                .await
                .expect("load BM25")
                .expect("present BM25"),
        ),
        Kind::Hnsw => Loaded::Hnsw(
            persistence
                .load_hnsw_snapshot()
                .await
                .expect("load HNSW")
                .expect("present HNSW"),
        ),
    }
}

fn check_loaded(loaded: &Loaded, expected: &Indexes) {
    match loaded {
        Loaded::Bm25(index) => {
            assert_eq!(index.doc_count(), ROWS);
            assert_eq!(
                serde_json::to_value(index).expect("BM25 content"),
                expected.bm25_value
            );
        }
        Loaded::Hnsw(snapshot) => {
            snapshot.verify().expect("snapshot integrity");
            assert_eq!(snapshot.total_nodes, ROWS);
            assert_eq!(snapshot.live_nodes, ROWS);
            assert_eq!(
                serde_json::to_value(snapshot).expect("HNSW content"),
                expected.hnsw_value
            );
        }
    }
}

fn check_shadow(result: ShadowValidationResult, kind: Kind) {
    assert!(
        result.passed,
        "shadow discrepancies: {:?}",
        result.discrepancies
    );
    assert_eq!(result.index_type, kind.name());
    assert_eq!(result.expected.item_count, ROWS);
    assert_eq!(result.expected.tombstone_count, 0);
    let actual = result.actual.expect("validation performed");
    assert_eq!(actual.item_count, ROWS);
    assert_eq!(actual.tombstone_count, 0);
    assert!(result.discrepancies.is_empty());
}

async fn iteration(kind: Kind, scenario: Scenario, current: &Indexes, old: &Indexes) -> Duration {
    let fixture = Fixture::new();
    let sentinel = RetrievalPersistence::new(fixture.backend.sql(), OTHER);
    let config = ShadowValidationConfig::enabled();
    if !matches!(scenario, Scenario::Fresh) {
        save(&sentinel, kind, old).await;
        let prior = RetrievalPersistence::new(fixture.backend.sql(), NAMESPACE);
        save(&prior, kind, old).await;
        check_loaded(&load(&prior, kind).await, old);
    }

    let elapsed = match scenario {
        Scenario::Fresh => {
            let start = Instant::now();
            let persistence = RetrievalPersistence::new(fixture.backend.sql(), NAMESPACE);
            save(&persistence, kind, black_box(current)).await;
            let loaded = load(&persistence, kind).await;
            let elapsed = start.elapsed();
            check_loaded(&loaded, current);
            black_box(loaded);
            elapsed
        }
        Scenario::Replace => {
            let persistence = RetrievalPersistence::new(fixture.backend.sql(), NAMESPACE);
            let start = Instant::now();
            save(&persistence, kind, black_box(current)).await;
            let loaded = load(&persistence, kind).await;
            let elapsed = start.elapsed();
            check_loaded(&loaded, current);
            black_box(loaded);
            elapsed
        }
        Scenario::Shadow => {
            let persistence = RetrievalPersistence::new(fixture.backend.sql(), NAMESPACE);
            let start = Instant::now();
            let result = match kind {
                Kind::Bm25 => {
                    persistence
                        .persist_bm25_with_validation(black_box(&current.bm25), &config)
                        .await
                }
                Kind::Hnsw => {
                    persistence
                        .persist_hnsw_with_validation(black_box(&current.hnsw), &config)
                        .await
                }
            }
            .expect("shadow save")
            .expect("sample_rate=1 must validate");
            let elapsed = start.elapsed();
            check_shadow(result, kind);
            // Wrapper already loaded inside timing; this exact-content check is untimed.
            check_loaded(&load(&persistence, kind).await, current);
            elapsed
        }
    };
    let expected_rows = if matches!(scenario, Scenario::Fresh) {
        1
    } else {
        check_loaded(&load(&sentinel, kind).await, old);
        2
    };
    let rows = fixture
        .backend
        .reader()
        .await
        .expect("snapshot census reader")
        .query_all(SqlStatement::new(
            "SELECT namespace, index_type FROM retrieval_snapshots",
            vec![],
        ))
        .await
        .expect("snapshot census");
    assert_eq!(
        rows.len(),
        expected_rows,
        "replacement keeps one row per namespace and kind"
    );
    drop(sentinel);
    fixture.close().await;
    elapsed
}

fn persistence(c: &mut Criterion) {
    let inputs = OnceCell::new();
    let get_inputs = || {
        inputs.get_or_init(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            (
                rt,
                Indexes::generated(42, 0),
                Indexes::generated(43, 10_000),
            )
        })
    };
    if mode::is_profile_mode() {
        get_inputs();
    }
    let mut group = c.benchmark_group("persistence");
    group.sample_size(10);
    for kind in [Kind::Bm25, Kind::Hnsw] {
        for scenario in [Scenario::Fresh, Scenario::Replace, Scenario::Shadow] {
            group.bench_function(BenchmarkId::new(kind.name(), scenario.name()), |b| {
                // Ordinary selection stays lazy; profiling prewarms outside its callback.
                let (rt, current, old) = get_inputs();
                // Sequential setup prevents Criterion warmup from accumulating fixtures.
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        elapsed += rt.block_on(iteration(kind, scenario, current, old));
                    }
                    elapsed
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, persistence);
criterion_main!(benches);
