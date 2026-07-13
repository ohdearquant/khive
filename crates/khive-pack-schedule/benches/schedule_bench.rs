use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use serde_json::json;

use khive_pack_kg::KgPack;
use khive_pack_schedule::SchedulePack;
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
    builder.register(khive_pack_comm::CommPack::new(khive_rt.clone()));
    builder.register(SchedulePack::new(khive_rt.clone()));
    let registry = builder.build().expect("registry builds");
    Fixture { registry, rt }
}

/// Seed `n` reminder events with deterministic future timestamps.
///
/// Timestamps are spread across years 2100–2299 to ensure they stay in the
/// future without relying on wall-clock offsets.
fn seed_events(fixture: &Fixture, n: usize) {
    fixture.rt.block_on(async {
        for i in 0..n {
            // Year 2100 + i spread across months. Max i=199 gives year 2116.
            let year = 2100 + (i / 12);
            let month = (i % 12) + 1;
            let at = format!("{year}-{month:02}-01T00:00:00Z");
            fixture
                .registry
                .dispatch(
                    "schedule.remind",
                    json!({
                        "content": format!("event-{i}"),
                        "at": at,
                    }),
                )
                .await
                .expect("seed remind");
        }
    });
}

// ── remind ────────────────────────────────────────────────────────────────────

fn bench_remind(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    group.sample_size(50);

    // Use iter_batched with a fresh fixture per batch so the measured
    // dispatch always writes into an empty store (no growing-store drift).
    group.bench_function("remind", |b| {
        b.iter_batched(
            build_fixture,
            |fixture| {
                fixture.rt.block_on(async {
                    let result = fixture
                        .registry
                        .dispatch(
                            "schedule.remind",
                            black_box(json!({
                                "content": "benchmark reminder",
                                "at": "2199-01-01T00:00:00Z"
                            })),
                        )
                        .await
                        .expect("remind ok");
                    black_box(result)
                })
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── schedule ──────────────────────────────────────────────────────────────────

fn bench_schedule(c: &mut Criterion) {
    let fixture = build_fixture();
    let mut group = c.benchmark_group("schedule");
    group.sample_size(50);

    group.bench_function("schedule", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            async move {
                let result = registry
                    .dispatch(
                        "schedule.schedule",
                        black_box(json!({
                            "action": "remind(content=\"scheduled action\")",
                            "at": "2199-06-01T12:00:00Z"
                        })),
                    )
                    .await
                    .expect("schedule ok");
                black_box(result)
            }
        });
    });

    group.finish();
}

// ── agenda ────────────────────────────────────────────────────────────────────

fn bench_agenda(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    group.sample_size(50);

    for &n_events in &[10usize, 100] {
        let fixture = build_fixture();
        seed_events(&fixture, n_events);

        group.bench_with_input(BenchmarkId::new("agenda", n_events), &n_events, |b, _n| {
            b.to_async(&fixture.rt).iter(|| {
                let registry = &fixture.registry;
                async move {
                    let result = registry
                        .dispatch("schedule.agenda", black_box(json!({ "limit": 20 })))
                        .await
                        .expect("agenda ok");
                    black_box(result)
                }
            });
        });
    }

    group.finish();
}

// ── cancel ────────────────────────────────────────────────────────────────────

fn bench_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    group.sample_size(50);

    // Each iteration gets a fresh fixture and a pre-created event ID in setup;
    // only schedule.cancel is timed.  No shared state accumulates across iters.
    group.bench_function("cancel", |b| {
        b.iter_batched(
            || {
                let fixture = build_fixture();
                let full_id = fixture.rt.block_on(async {
                    let created = fixture
                        .registry
                        .dispatch(
                            "schedule.remind",
                            json!({ "content": "cancel-target", "at": "2199-03-01T00:00:00Z" }),
                        )
                        .await
                        .expect("setup remind");
                    created["full_id"].as_str().expect("full_id").to_owned()
                });
                (fixture, full_id)
            },
            |(fixture, full_id)| {
                fixture.rt.block_on(async {
                    let result = fixture
                        .registry
                        .dispatch("schedule.cancel", black_box(json!({ "id": full_id })))
                        .await
                        .expect("cancel ok");
                    black_box(result)
                })
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    schedule_benches,
    bench_remind,
    bench_schedule,
    bench_agenda,
    bench_cancel
);
criterion_main!(schedule_benches);
