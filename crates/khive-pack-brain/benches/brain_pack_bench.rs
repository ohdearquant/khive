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

/// Create a section-enabled profile through brain.create_profile and return
/// its profile_id.  The profile is inserted into section_states by the
/// handler, which means subsequent brain.feedback calls with
/// `served_by_profile_id` set to this id will exercise the real
/// SectionPosteriorState::apply_signal path.
fn create_section_profile(registry: &VerbRegistry, rt_tokio: &tokio::runtime::Runtime) -> String {
    rt_tokio
        .block_on(async {
            registry
                .dispatch(
                    "brain.create_profile",
                    json!({
                        "name": "bench-section-profile",
                        "consumer_kind": "agent"
                    }),
                )
                .await
        })
        .expect("brain.create_profile must succeed")
        .get("profile_id")
        .and_then(|v| v.as_str())
        .expect("create_profile must return profile_id")
        .to_string()
}

// ── brain.feedback — with section_signals (capped path) ──────────────────────

fn bench_feedback_with_sections(c: &mut Criterion) {
    let mut group = c.benchmark_group("brain_pack/feedback_sections");
    group.sample_size(50);

    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");

    // Primed setup: create a section-enabled profile, warm up its section state
    // with 200 positive events outside the timed closure, then measure the cost
    // of 200 opposing events.  This is the path where apply_ess_cap fires on
    // every SectionPosteriorState update — the capped path that was previously
    // bypassed because no profile was in section_states.
    group.bench_function("capped_200_opposing", |b| {
        b.iter_batched(
            || {
                let registry = build_registry();
                let target_id = create_target(&registry, &rt_tokio);
                // Create a section-enabled profile so section_states has an
                // entry for this profile_id.
                let profile_id = create_section_profile(&registry, &rt_tokio);
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
                                        "served_by_profile_id": profile_id,
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
                (registry, target_id, profile_id)
            },
            |(registry, target_id, profile_id)| {
                // Measured: 200 opposing section events; ESS cap fires each time.
                rt_tokio.block_on(async {
                    for _ in 0..200 {
                        registry
                            .dispatch(
                                "brain.feedback",
                                json!({
                                    "target_id": target_id,
                                    "signal": "not_useful",
                                    "served_by_profile_id": profile_id,
                                    "section_signals": {
                                        "operational_guidance": "not_useful",
                                        "examples": "not_useful"
                                    }
                                }),
                            )
                            .await
                            .expect("feedback must succeed");
                    }
                    black_box(profile_id)
                })
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

/// Read the `operational_guidance` mean from `brain.profile` for a given
/// profile_id.  Returns 0.5 (the Beta(1,1) prior) if the field is absent.
fn read_og_mean(
    registry: &VerbRegistry,
    rt_tokio: &tokio::runtime::Runtime,
    profile_id: &str,
) -> f64 {
    let resp = rt_tokio
        .block_on(async {
            registry
                .dispatch("brain.profile", json!({ "profile_id": profile_id }))
                .await
        })
        .expect("brain.profile must succeed");
    resp.get("section_posteriors")
        .and_then(|sp| sp.get("operational_guidance"))
        .and_then(|s| s.get("mean"))
        .and_then(|m| m.as_f64())
        .unwrap_or(0.5)
}

/// Correctness gate: assert that the section posterior mean for
/// `operational_guidance` shifts by ≥ 0.3 after 200 positive then 200
/// opposing events routed through a section-enabled profile.
///
/// Panics if the shift is < 0.3, which would mean `section_states` was not
/// updated — the silent-no-op regression this bench was written to catch.
fn assert_section_posterior_shifted(rt_tokio: &tokio::runtime::Runtime) {
    let registry = build_registry();
    let target_id = create_target(&registry, rt_tokio);
    let profile_id = create_section_profile(&registry, rt_tokio);

    // Read prior mean before any feedback.
    let prior_mean = read_og_mean(&registry, rt_tokio, &profile_id);

    // Apply 200 positive section events.
    for _ in 0..200 {
        rt_tokio
            .block_on(async {
                registry
                    .dispatch(
                        "brain.feedback",
                        json!({
                            "target_id": target_id,
                            "signal": "useful",
                            "served_by_profile_id": profile_id,
                            "section_signals": { "operational_guidance": "useful" }
                        }),
                    )
                    .await
            })
            .expect("positive feedback");
    }
    let mean_after_positive = read_og_mean(&registry, rt_tokio, &profile_id);

    // Apply 200 opposing section events.
    for _ in 0..200 {
        rt_tokio
            .block_on(async {
                registry
                    .dispatch(
                        "brain.feedback",
                        json!({
                            "target_id": target_id,
                            "signal": "not_useful",
                            "served_by_profile_id": profile_id,
                            "section_signals": { "operational_guidance": "not_useful" }
                        }),
                    )
                    .await
            })
            .expect("opposing feedback");
    }
    let mean_after_opposing = read_og_mean(&registry, rt_tokio, &profile_id);

    let shift = (mean_after_positive - mean_after_opposing).abs();
    assert!(
        shift >= 0.3,
        "section posterior correctness gate: operational_guidance mean shift {shift:.4} < 0.3 \
         (prior={prior_mean:.4}, after_pos={mean_after_positive:.4}, \
         after_opp={mean_after_opposing:.4}); \
         section_states was not updated — the section posterior path is broken"
    );
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

// ── section posterior correctness gate ───────────────────────────────────────
//
// Registered as a Criterion benchmark so it runs under `cargo bench` and fails
// the bench binary (not just a debug build) when the section posterior path is
// broken.  The body does zero iterations of actual timing work — it exists to
// enforce the invariant that brain.feedback with served_by_profile_id actually
// updates section_states.

fn bench_section_posterior_gate(c: &mut Criterion) {
    let rt_tokio = tokio::runtime::Runtime::new().expect("tokio");
    // Run the correctness assertion once before any timed work.
    assert_section_posterior_shifted(&rt_tokio);
    // One trivial timed iteration so Criterion records a result.
    c.benchmark_group("brain_pack/section_posterior_gate")
        .bench_function("mean_shift_ge_0_3", |b| b.iter(|| black_box(())));
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_feedback_plain,
    bench_feedback_with_sections,
    bench_section_posterior_gate,
    bench_profile_read,
    bench_reset,
    bench_snapshot_roundtrip,
);
criterion_main!(benches);
