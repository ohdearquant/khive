use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;

use khive_pack_comm::CommPack;
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
    builder.register(CommPack::new(khive_rt.clone()));
    builder.with_default_namespace("local");
    let registry = builder.build().expect("registry builds");
    Fixture { registry, rt }
}

/// Seed `n` self-send messages so read/inbox/thread benchmarks have realistic data.
///
/// Returns the full UUIDs of all inbound copies (needed for read/reply/thread).
fn seed_messages(fixture: &Fixture, n: usize) -> Vec<String> {
    fixture.rt.block_on(async {
        let mut inbound_ids: Vec<String> = Vec::with_capacity(n);
        for i in 0..n {
            fixture
                .registry
                .dispatch(
                    "comm.send",
                    json!({
                        "to": "local",
                        "content": format!("message body number {i}"),
                        "subject": format!("Subject {i}"),
                    }),
                )
                .await
                .expect("seed send");
        }
        // Retrieve all inbound copies via inbox(status=all).
        let inbox = fixture
            .registry
            .dispatch("comm.inbox", json!({ "status": "all", "limit": 200 }))
            .await
            .expect("inbox for seeding");
        if let Some(msgs) = inbox.get("messages").and_then(|v| v.as_array()) {
            for msg in msgs {
                if let Some(id) = msg.get("full_id").and_then(|v| v.as_str()) {
                    inbound_ids.push(id.to_string());
                }
            }
        }
        inbound_ids
    })
}

// ── send ─────────────────────────────────────────────────────────────────────

fn bench_send(c: &mut Criterion) {
    let fixture = build_fixture();
    let mut group = c.benchmark_group("comm");
    group.sample_size(50);

    group.bench_function("send", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            async move {
                let result = registry
                    .dispatch(
                        "comm.send",
                        black_box(json!({
                            "to": "local",
                            "content": "benchmark message content",
                            "subject": "Bench Subject",
                        })),
                    )
                    .await
                    .expect("send ok");
                black_box(result)
            }
        });
    });

    group.finish();
}

// ── inbox ─────────────────────────────────────────────────────────────────────

fn bench_inbox(c: &mut Criterion) {
    let mut group = c.benchmark_group("comm");
    group.sample_size(50);

    for &n_messages in &[10usize, 100] {
        let fixture = build_fixture();
        seed_messages(&fixture, n_messages);

        group.bench_with_input(
            BenchmarkId::new("inbox", n_messages),
            &n_messages,
            |b, _n| {
                b.to_async(&fixture.rt).iter(|| {
                    let registry = &fixture.registry;
                    async move {
                        let result = registry
                            .dispatch(
                                "comm.inbox",
                                black_box(json!({ "status": "all", "limit": 20 })),
                            )
                            .await
                            .expect("inbox ok");
                        black_box(result)
                    }
                });
            },
        );
    }

    group.finish();
}

// ── read ─────────────────────────────────────────────────────────────────────

fn bench_read(c: &mut Criterion) {
    let fixture = build_fixture();
    let inbound_ids = seed_messages(&fixture, 200);
    let ids = std::sync::Arc::new(std::sync::Mutex::new(inbound_ids.into_iter().cycle()));

    let mut group = c.benchmark_group("comm");
    group.sample_size(50);

    group.bench_function("read", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            let full_id = ids.lock().expect("ids lock").next().expect("cycle");
            async move {
                let _ = registry
                    .dispatch("comm.read", black_box(json!({ "id": full_id })))
                    .await;
                black_box(())
            }
        });
    });

    group.finish();
}

// ── reply ─────────────────────────────────────────────────────────────────────

fn bench_reply(c: &mut Criterion) {
    let fixture = build_fixture();
    let inbound_ids = seed_messages(&fixture, 200);
    let ids = std::sync::Arc::new(std::sync::Mutex::new(inbound_ids.into_iter().cycle()));

    let mut group = c.benchmark_group("comm");
    group.sample_size(50);

    group.bench_function("reply", |b| {
        b.to_async(&fixture.rt).iter(|| {
            let registry = &fixture.registry;
            let full_id = ids.lock().expect("ids lock").next().expect("cycle");
            async move {
                let result = registry
                    .dispatch(
                        "comm.reply",
                        black_box(json!({ "id": full_id, "content": "bench reply" })),
                    )
                    .await
                    .expect("reply ok");
                black_box(result)
            }
        });
    });

    group.finish();
}

// ── thread ────────────────────────────────────────────────────────────────────

fn bench_thread(c: &mut Criterion) {
    let fixture = build_fixture();

    // Build a thread with 5 messages: 1 root + 4 replies.
    let thread_root_id = fixture.rt.block_on(async {
        let root = fixture
            .registry
            .dispatch(
                "comm.send",
                json!({ "to": "local", "content": "thread root message" }),
            )
            .await
            .expect("send thread root");
        let root_full_id = root
            .get("full_id")
            .and_then(|v| v.as_str())
            .expect("root full_id")
            .to_string();

        for i in 1..5 {
            fixture
                .registry
                .dispatch(
                    "comm.reply",
                    json!({ "id": root_full_id, "content": format!("reply {i}") }),
                )
                .await
                .expect("seed reply");
        }
        root_full_id
    });

    let mut group = c.benchmark_group("comm");
    group.sample_size(50);

    group.bench_with_input(
        BenchmarkId::new("thread", 5),
        &thread_root_id,
        |b, root_id| {
            b.to_async(&fixture.rt).iter(|| {
                let registry = &fixture.registry;
                let id = root_id.clone();
                async move {
                    let result = registry
                        .dispatch("comm.thread", black_box(json!({ "id": id })))
                        .await
                        .expect("thread ok");
                    black_box(result)
                }
            });
        },
    );

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    comm_benches,
    bench_send,
    bench_inbox,
    bench_read,
    bench_reply,
    bench_thread
);
criterion_main!(comm_benches);
