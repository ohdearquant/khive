use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

// ── fixture ───────────────────────────────────────────────────────────────────

struct Fixture {
    registry: VerbRegistry,
    rt: tokio::runtime::Runtime,
}

fn build_fixture() -> Fixture {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let khive_rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(khive_rt.clone()));
    builder.register(GtdPack::new(khive_rt.clone()));
    let registry = builder.build().expect("registry builds");
    khive_rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry, rt }
}

/// Seed `n` tasks with mixed statuses so read benchmarks have realistic data.
fn seed_tasks(fixture: &Fixture, n: usize) {
    let statuses = ["inbox", "next", "active", "waiting", "someday"];
    let priorities = ["p0", "p1", "p2", "p3"];
    fixture.rt.block_on(async {
        for i in 0..n {
            let status = statuses[i % statuses.len()];
            let priority = priorities[i % priorities.len()];
            fixture
                .registry
                .dispatch(
                    "gtd.assign",
                    json!({
                        "title": format!("task-{i}"),
                        "status": status,
                        "priority": priority,
                    }),
                )
                .await
                .expect("seed assign");
        }
    });
}

// ── assign ────────────────────────────────────────────────────────────────────

fn bench_assign(c: &mut Criterion) {
    let fixture = build_fixture();
    let mut group = c.benchmark_group("gtd");
    group.sample_size(50);

    group.bench_function("assign", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            async move {
                let result = registry
                    .dispatch(
                        "gtd.assign",
                        black_box(json!({ "title": "benchmark task", "priority": "p2" })),
                    )
                    .await
                    .expect("assign ok");
                black_box(result)
            }
        });
    });

    group.finish();
}

// ── next ──────────────────────────────────────────────────────────────────────

fn bench_next(c: &mut Criterion) {
    let mut group = c.benchmark_group("gtd");
    group.sample_size(50);

    for &n_tasks in &[10usize, 100] {
        let fixture = build_fixture();
        seed_tasks(&fixture, n_tasks);

        group.bench_with_input(BenchmarkId::new("next", n_tasks), &n_tasks, |b, _n| {
            b.to_async(&fixture.rt).iter(|| {
                let registry = &fixture.registry;
                async move {
                    let result = registry
                        .dispatch("gtd.next", black_box(json!({ "limit": 10 })))
                        .await
                        .expect("next ok");
                    black_box(result)
                }
            });
        });
    }

    group.finish();
}

// ── tasks/filter_by_status ────────────────────────────────────────────────────

fn bench_tasks(c: &mut Criterion) {
    let fixture = build_fixture();
    seed_tasks(&fixture, 100);

    let mut group = c.benchmark_group("gtd");
    group.sample_size(50);

    group.bench_function("tasks/filter_by_status", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            async move {
                let result = registry
                    .dispatch(
                        "gtd.tasks",
                        black_box(json!({ "status": "next", "limit": 50 })),
                    )
                    .await
                    .expect("tasks ok");
                black_box(result)
            }
        });
    });

    group.finish();
}

// ── transition ────────────────────────────────────────────────────────────────

fn bench_transition(c: &mut Criterion) {
    let fixture = build_fixture();
    let mut group = c.benchmark_group("gtd");
    group.sample_size(50);
    let counter = std::sync::atomic::AtomicU64::new(0);

    group.bench_function("transition", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            let i = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            async move {
                let created = registry
                    .dispatch(
                        "gtd.assign",
                        json!({ "title": format!("t-{i}"), "status": "inbox" }),
                    )
                    .await
                    .expect("assign");
                let full_id = created["full_id"].as_str().expect("full_id");
                let result = registry
                    .dispatch(
                        "gtd.transition",
                        black_box(json!({ "id": full_id, "status": "next" })),
                    )
                    .await
                    .expect("transition ok");
                black_box(result)
            }
        });
    });

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    gtd_benches,
    bench_assign,
    bench_next,
    bench_tasks,
    bench_transition
);
criterion_main!(gtd_benches);
