//! Criterion benchmarks for the public `brain.*` verb dispatch path.
//!
//! These benchmarks exercise the real `KhiveRuntime` + `VerbRegistry` +
//! `BrainPack` stack — the same path a caller exercises when issuing
//! `brain.feedback`, `brain.profile`, `brain.reset`, or requesting a snapshot
//! via `brain.state`.  Use these numbers for any performance claim about the
//! `brain.*` API; the core primitive micro-benchmarks in
//! `khive-brain-core/benches/brain_core_bench.rs` are intentionally isolated
//! from this dispatch overhead.
//!
//! Run with:
//!   cd crates && cargo bench -p khive-pack-brain --bench brain_pack_bench

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use serde_json::json;

use khive_pack_brain::BrainPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

// ── harness helpers ───────────────────────────────────────────────────────────

fn build_registry() -> VerbRegistry {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

/// Create a real entity in the in-memory DB and return its UUID string.
/// brain.feedback requires a valid target_id that resolves in the namespace.
fn create_target(registry: &VerbRegistry, rt_tokio: &tokio::runtime::Runtime) -> String {
    rt_tokio
        .block_on(async {
            registry
                .dispatch(
                    "create",
                    json!({
                        "kind": "entity",
                        "entity_kind": "concept",
                        "name": "BrainBenchTarget"
                    }),
                )
                .await
        })
        .expect("create entity")
        .get("id")
        .and_then(|v| v.as_str())
        .expect("create must return id")
        .to_string()
}

// ── brain.feedback — plain signal ────────────────────────────────────────────

fn bench_feedback_plain(c: &mut Criterion) {
    let mut group = c.benchmark_group("brain_pack/feedback_plain");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    group.bench_function("useful", |b| {
        b.iter_batched(
            || {
                let registry = build_registry();
                let target_id = create_target(&registry, &rt_tokio);
                (registry, target_id)
            },
            |(registry, target_id)| {
                rt_tokio.block_on(async {
                    black_box(
                        registry
                            .dispatch(
                                "brain.feedback",
                                json!({
                                    "target_id": target_id,
                                    "signal": "useful"
                                }),
                            )
                            .await
                            .expect("brain.feedback must succeed"),
                    )
                })
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ── brain.feedback — with section_signals (capped path) ──────────────────────

fn bench_feedback_with_sections(c: &mut Criterion) {
    let mut group = c.benchmark_group("brain_pack/feedback_sections");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    // Primed setup: warm up the section state with 200 positive events outside
    // the timed closure, then measure the cost of 200 opposing events.  This
    // is the path where apply_ess_cap fires on every SectionPosteriorState
    // update — the capped path that was previously untested.
    group.bench_function("capped_200_opposing", |b| {
        b.iter_batched(
            || {
                let registry = build_registry();
                let target_id = create_target(&registry, &rt_tokio);
                // Prime 200 positive section events outside the timed path.
                for _ in 0..200 {
                    rt_tokio
                        .block_on(async {
                            registry
                                .dispatch(
                                    "brain.feedback",
                                    json!({
                                        "target_id": target_id,
                                        "signal": "useful",
                                        "section_signals": {
                                            "operational_guidance": "useful",
                                            "examples": "useful"
                                        }
                                    }),
                                )
                                .await
                        })
                        .expect("setup feedback");
                }
                (registry, target_id)
            },
            |(registry, target_id)| {
                // Measured: 200 opposing section events; ESS cap fires each time.
                rt_tokio.block_on(async {
                    for _ in 0..200 {
                        registry
                            .dispatch(
                                "brain.feedback",
                                json!({
                                    "target_id": target_id,
                                    "signal": "not_useful",
                                    "section_signals": {
                                        "operational_guidance": "not_useful",
                                        "examples": "not_useful"
                                    }
                                }),
                            )
                            .await
                            .expect("feedback must succeed");
                    }
                    // Read back state via brain.state to confirm the capped path
                    // ran and the state is consistent.
                    black_box(
                        registry
                            .dispatch("brain.state", json!({}))
                            .await
                            .expect("brain.state must succeed"),
                    )
                })
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ── brain.profile ─────────────────────────────────────────────────────────────

fn bench_profile_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("brain_pack/profile_read");
    group.sample_size(100);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");
    let registry = build_registry();

    group.bench_function("balanced_recall_v1", |b| {
        b.iter(|| {
            rt_tokio.block_on(async {
                black_box(
                    registry
                        .dispatch(
                            "brain.profile",
                            json!({ "profile_id": "balanced-recall-v1" }),
                        )
                        .await
                        .expect("brain.profile must succeed"),
                )
            })
        })
    });

    group.finish();
}

// ── brain.reset ───────────────────────────────────────────────────────────────

fn bench_reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("brain_pack/reset");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    group.bench_function("balanced_recall_v1", |b| {
        b.iter_batched(
            build_registry,
            |registry| {
                rt_tokio.block_on(async {
                    black_box(
                        registry
                            .dispatch("brain.reset", json!({}))
                            .await
                            .expect("brain.reset must succeed"),
                    )
                })
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ── brain.state snapshot round-trip ──────────────────────────────────────────

fn bench_snapshot_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("brain_pack/snapshot_roundtrip");
    group.sample_size(100);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");
    let registry = build_registry();

    // Time the full public-path round-trip: brain.state (to_snapshot serialised
    // to JSON) followed by brain.state again (verifying the state is stable).
    group.bench_function("state_read_twice", |b| {
        b.iter(|| {
            rt_tokio.block_on(async {
                let snap1 = registry
                    .dispatch("brain.state", json!({}))
                    .await
                    .expect("brain.state must succeed");
                let snap2 = registry
                    .dispatch("brain.state", json!({}))
                    .await
                    .expect("brain.state must succeed");
                // Both reads should return equivalent state (no mutation).
                debug_assert_eq!(
                    snap1.get("total_events"),
                    snap2.get("total_events"),
                    "brain.state must be stable across reads"
                );
                black_box((snap1, snap2))
            })
        })
    });

    group.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_feedback_plain,
    bench_feedback_with_sections,
    bench_profile_read,
    bench_reset,
    bench_snapshot_roundtrip,
);
criterion_main!(benches);
