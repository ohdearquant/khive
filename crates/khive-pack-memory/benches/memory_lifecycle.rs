//! Feedback, coherent pruning and top-level vacuum on generated private fixtures.
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

mod lifecycle_fixture;
use lifecycle_fixture::{check_recall, executor, report, rows, Fixture, QUERY, SAMPLES};

async fn feedback() -> Duration {
    let fixture = Fixture::new(10, false, false).await;
    let recall = fixture
        .registry
        .dispatch("memory.recall", json!({"query":QUERY,"limit":1}))
        .await
        .expect("recall target");
    check_recall(&recall);
    let target = rows(&recall)[0]["full_id"]
        .as_str()
        .expect("target")
        .to_owned();
    assert_eq!(
        Uuid::parse_str(&target).expect("full UUID").to_string(),
        target
    );
    let params = json!({"target_id":target,"signal":"useful"});
    let start = Instant::now();
    let response = fixture
        .registry
        .dispatch("memory.feedback", black_box(params))
        .await
        .expect("feedback");
    let elapsed = start.elapsed();
    assert_eq!(response["ok"], true);
    assert_eq!(response["target_id"], target);
    assert_eq!(response["signal"], "useful");
    let _ = fixture.close().await;
    elapsed
}

async fn scalar(fixture: &Fixture, sql: &str) -> i64 {
    match fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("accounting reader")
        .query_scalar(SqlStatement::new(sql, vec![]))
        .await
        .expect("accounting query")
    {
        Some(SqlValue::Integer(value)) => value,
        value => panic!("expected integer: {value:?}"),
    }
}

async fn counts(fixture: &Fixture) -> (i64, i64) {
    let total = scalar(fixture, "SELECT COUNT(*) FROM notes WHERE kind = 'memory'").await;
    let deleted = scalar(
        fixture,
        "SELECT COUNT(*) FROM notes WHERE kind = 'memory' AND deleted_at IS NOT NULL",
    )
    .await;
    (total, deleted)
}

async fn prune_call(fixture: &Fixture) -> Duration {
    let params = json!({"min_salience":0.5,"before":0,"dry_run":false});
    let start = Instant::now();
    let response = fixture
        .registry
        .dispatch("memory.prune", black_box(params))
        .await
        .expect("prune");
    let elapsed = start.elapsed();
    assert_eq!(response["pruned"], 250);
    assert_eq!(response["dry_run"], false);
    assert_eq!(counts(fixture).await, (500, 250));
    elapsed
}

async fn prune() -> Duration {
    let fixture = Fixture::new(500, false, false).await;
    assert_eq!(counts(&fixture).await, (500, 0));
    let elapsed = prune_call(&fixture).await;
    let _ = fixture.close().await;
    elapsed
}

async fn accounting(fixture: &Fixture) -> Value {
    let pages = scalar(fixture, "PRAGMA page_count").await;
    let page_size = scalar(fixture, "PRAGMA page_size").await;
    let free_pages = scalar(fixture, "PRAGMA freelist_count").await;
    assert!(pages > 0 && page_size > 0 && free_pages >= 0);
    json!({"page_count":pages,"page_size":page_size,"logical_bytes":pages.checked_mul(page_size).expect("page accounting"),"freelist_count":free_pages})
}

async fn vacuum(print_accounting: bool) -> Duration {
    let fixture = Fixture::new(500, true, false).await;
    let _ = prune_call(&fixture).await;
    let before = accounting(&fixture).await;
    let params = json!({});
    let start = Instant::now();
    let response = fixture
        .registry
        .dispatch("memory.vacuum", black_box(params))
        .await
        .expect("top-level vacuum");
    let elapsed = start.elapsed();
    assert_eq!(response["ok"], true);
    assert_eq!(
        counts(&fixture).await,
        (500, 250),
        "soft deletes remain records after vacuum"
    );
    let after = accounting(&fixture).await;
    let settled_bytes = fixture.close().await.expect("file-backed byte count");
    assert!(settled_bytes > 0);
    if print_accounting {
        println!(
            "{}",
            json!({"scenario":"memory_vacuum/vacuum_500_half_deleted","wall_ms":elapsed.as_secs_f64()*1000.0,"before":before,"after":after,"settled_database_bytes":settled_bytes})
        );
    }
    elapsed
}

fn lifecycle(c: &mut Criterion) {
    let rt = executor();
    let samples = (0..SAMPLES).map(|_| rt.block_on(feedback())).collect();
    report("memory_lifecycle/feedback_fresh", samples);
    let _ = rt.block_on(vacuum(true));
    let mut group = c.benchmark_group("memory_lifecycle");
    group.sample_size(10);
    group.bench_function("feedback_fresh", |b| {
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| rt.block_on(feedback()))
                .sum::<Duration>()
        })
    });
    group.throughput(Throughput::Elements(250));
    group.bench_function("prune_500_half_low", |b| {
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| rt.block_on(prune()))
                .sum::<Duration>()
        })
    });
    group.finish();
    // Separate group prevents the prune row-rate unit from leaking into vacuum.
    let mut group = c.benchmark_group("memory_vacuum");
    group.sample_size(10);
    group.bench_function("vacuum_500_half_deleted", |b| {
        b.iter_custom(|iterations| {
            (0..iterations)
                .map(|_| rt.block_on(vacuum(false)))
                .sum::<Duration>()
        })
    });
    group.finish();
}

criterion_group!(benches, lifecycle);
criterion_main!(benches);
