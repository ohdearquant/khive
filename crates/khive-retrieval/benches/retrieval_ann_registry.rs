//! Execute pathless statements against fresh private in-memory SQLite fixtures.
use std::cell::OnceCell;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_db::StorageBackend;
use khive_retrieval::ann::registry::{
    pathless_compact_log, pathless_mark_recovering, pathless_raise_watermark,
    pathless_register_pending, CompactionScope, WatermarkAuthority,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SqlAccess;

#[path = "fixtures/mode.rs"]
mod mode;

const NAMESPACE: &str = "bench:registry";
const MODEL: &str = "bench-model";
const PREFIX: &str = "bench_";

#[derive(Clone, Copy)]
enum Scenario {
    Register,
    Recover,
    Compact,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::Register => "register_raise_compact",
            Self::Recover => "recover_raise_compact",
            Self::Compact => "compact_only",
        }
    }
}

async fn execute(sql: &dyn SqlAccess, statement: SqlStatement) -> u64 {
    sql.writer()
        .await
        .expect("admit writer")
        .execute(statement)
        .await
        .expect("execute statement")
}

async fn seed(sql: &dyn SqlAccess, entries: usize) {
    for seq in 1..=entries {
        let inserted = execute(sql, SqlStatement::new(
            "INSERT INTO ann_write_log (seq, namespace, embedding_model, kind, field, subject_id, op) \
             VALUES (?1, ?2, ?3, 'note', 'note.content', ?4, 'upsert')",
            vec![SqlValue::Integer(seq as i64), SqlValue::Text(NAMESPACE.into()), SqlValue::Text(MODEL.into()), SqlValue::Text(format!("subject-{seq}"))],
        )).await;
        assert_eq!(inserted, 1);
    }
}

fn register(consumer: &str) -> SqlStatement {
    pathless_register_pending(PREFIX, consumer, NAMESPACE, MODEL)
}

fn raise(consumer: &str, watermark: i64, authority: WatermarkAuthority) -> SqlStatement {
    pathless_raise_watermark(PREFIX, consumer, NAMESPACE, MODEL, watermark, authority)
}

async fn check_result(sql: &dyn SqlAccess, entries: usize, watermark: i64) {
    let mut reader = sql.reader().await.expect("verify reader");
    let consumers = reader.query_all(SqlStatement::new(
        "SELECT consumer, watermark FROM ann_consumer_watermark WHERE namespace = ?1 AND embedding_model = ?2 ORDER BY consumer",
        vec![SqlValue::Text(NAMESPACE.into()), SqlValue::Text(MODEL.into())],
    )).await.expect("verify registry");
    assert_eq!(consumers.len(), entries);
    for (index, row) in consumers.iter().enumerate() {
        assert_eq!(
            row.text("consumer").expect("consumer"),
            format!("reader-{index:04}")
        );
        assert_eq!(row.i64("watermark").expect("watermark"), watermark);
    }
    let remaining = reader
        .query_all(SqlStatement::new(
            "SELECT seq FROM ann_write_log ORDER BY seq",
            vec![],
        ))
        .await
        .expect("verify compacted log");
    let actual: Vec<_> = remaining
        .iter()
        .map(|row| row.i64("seq").expect("sequence"))
        .collect();
    assert_eq!(actual, (watermark + 1..=entries as i64).collect::<Vec<_>>());
}

async fn iteration(entries: usize, scenario: Scenario) -> Duration {
    let backend = StorageBackend::memory().expect("private memory backend");
    // A forced process override must fail loudly, not degrade to another route.
    assert!(
        !backend.pool().write_queue_active(),
        "pathless benchmark requires the memory writer route"
    );
    backend
        .prepare_core_schema()
        .expect("migrated registry schema");
    let sql = backend.sql();
    seed(sql.as_ref(), entries).await;
    let consumers: Vec<_> = (0..entries).map(|i| format!("reader-{i:04}")).collect();
    let watermark = (entries / 2) as i64;
    if !matches!(scenario, Scenario::Register) {
        for consumer in &consumers {
            assert_eq!(execute(sql.as_ref(), register(consumer)).await, 1);
            let initial = if matches!(scenario, Scenario::Recover) {
                0
            } else {
                watermark
            };
            assert_eq!(
                execute(
                    sql.as_ref(),
                    raise(consumer, initial, WatermarkAuthority::PendingOrActive)
                )
                .await,
                1
            );
        }
    }

    let mut first_rows = 0;
    let mut raised_rows = 0;
    let start = Instant::now();
    if !matches!(scenario, Scenario::Compact) {
        for consumer in &consumers {
            let (statement, authority) = match scenario {
                Scenario::Register => (register(consumer), WatermarkAuthority::PendingOrActive),
                Scenario::Recover => (
                    pathless_mark_recovering(PREFIX, consumer, NAMESPACE, MODEL),
                    WatermarkAuthority::Recovering,
                ),
                Scenario::Compact => unreachable!("compaction-only has no consumer loop"),
            };
            first_rows += execute(sql.as_ref(), black_box(statement)).await;
            raised_rows += execute(sql.as_ref(), raise(consumer, watermark, authority)).await;
        }
    }
    let deleted = execute(
        sql.as_ref(),
        pathless_compact_log(PREFIX, CompactionScope::Namespace(NAMESPACE.into()), MODEL),
    )
    .await;
    let elapsed = start.elapsed();

    let expected_changes = if matches!(scenario, Scenario::Compact) {
        0
    } else {
        entries as u64
    };
    assert_eq!(first_rows, expected_changes);
    assert_eq!(raised_rows, expected_changes);
    assert_eq!(deleted, watermark as u64);
    check_result(sql.as_ref(), entries, watermark).await;
    assert!(backend.pool().take_writer_task_join().is_none());
    drop(sql);
    drop(backend);
    elapsed
}

fn registry(c: &mut Criterion) {
    let runtime = OnceCell::new();
    let get_runtime = || {
        runtime.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .max_blocking_threads(2)
                .enable_all()
                .build()
                .expect("runtime")
        })
    };
    if mode::is_profile_mode() {
        get_runtime();
    }
    let mut group = c.benchmark_group("ann_registry");
    group.sample_size(10);
    for entries in [100, 500] {
        for scenario in [Scenario::Register, Scenario::Recover, Scenario::Compact] {
            group.bench_function(BenchmarkId::new(scenario.name(), entries), |b| {
                let rt = get_runtime();
                b.iter_custom(|iterations| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iterations {
                        elapsed += rt.block_on(iteration(entries, scenario));
                    }
                    elapsed
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, registry);
criterion_main!(benches);
