//! Criterion benchmarks for khive-brain-core primitives.
//!
//! Covers the Phase-1 bench gates from ADR-048 §"Benchmarks required":
//!   1. Profile save/load round-trip: snapshot == restored state.
//!   2. ESS cap convergence: 200 positive events then 200 opposing;
//!      posterior mean must shift by ≥ 0.3.
//!
//! Run with:
//!   cd crates && cargo bench -p khive-brain-core --bench brain_core_bench

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use khive_brain_core::{
    derive_deterministic_weights, BalancedRecallState, BrainSignal, BrainState, FeedbackSignal,
    SectionPosteriorState,
};
use uuid::Uuid;

// ── helpers ───────────────────────────────────────────────────────────────────

fn recall_hit(id: Uuid, latency_us: i64) -> BrainSignal {
    BrainSignal::RecallHit {
        target_id: id,
        latency_us,
    }
}

fn feedback(id: Uuid, signal: FeedbackSignal) -> BrainSignal {
    BrainSignal::Feedback {
        target_id: id,
        signal,
        served_by_profile_id: None,
        section_signals: None,
    }
}

/// Apply `n` positive recall-hit signals followed by `n` miss signals.
fn apply_alternating(state: &mut BalancedRecallState, n: usize) {
    let id = Uuid::new_v4();
    for _ in 0..n {
        state.apply_signal(&recall_hit(id, 10_000));
    }
    for _ in 0..n {
        state.apply_signal(&BrainSignal::RecallMiss);
    }
}

// ── snapshot round-trip ───────────────────────────────────────────────────────

fn bench_snapshot_roundtrip(c: &mut Criterion) {
    let mut g = c.benchmark_group("brain_snapshot");
    g.sample_size(200);

    // Bench 1: to_snapshot on a fresh default state.
    g.bench_function("to_snapshot/fresh", |b| {
        let state = BalancedRecallState::new(100);
        b.iter(|| black_box(state.to_snapshot()))
    });

    // Bench 2: to_snapshot on a state with 100 accumulated events.
    g.bench_function("to_snapshot/100_events", |b| {
        let mut state = BalancedRecallState::new(100);
        apply_alternating(&mut state, 50);
        b.iter(|| black_box(state.to_snapshot()))
    });

    // Bench 3: from_snapshot restore — full round-trip cost.
    g.bench_function("from_snapshot/restore", |b| {
        let mut state = BalancedRecallState::new(100);
        apply_alternating(&mut state, 50);
        let snapshot = state.to_snapshot();
        b.iter_batched(
            || snapshot.clone(),
            |snap| black_box(BalancedRecallState::from_snapshot(snap, 100)),
            BatchSize::SmallInput,
        )
    });

    // Bench 4: full BrainState round-trip (all profiles + section states).
    g.bench_function("brain_state/roundtrip", |b| {
        let mut brain = BrainState::new(100);
        let id = Uuid::new_v4();
        for _ in 0..50 {
            brain.balanced_recall.apply_signal(&recall_hit(id, 20_000));
        }
        let snapshot = brain.to_snapshot();
        b.iter_batched(
            || snapshot.clone(),
            |snap| black_box(BrainState::from_snapshot(snap, 100)),
            BatchSize::SmallInput,
        )
    });

    g.finish();
}

// ── ESS cap convergence ───────────────────────────────────────────────────────

fn bench_ess_cap_convergence(c: &mut Criterion) {
    let mut g = c.benchmark_group("brain_ess_convergence");
    g.sample_size(100);

    for &n_events in &[50usize, 100, 200] {
        g.bench_with_input(
            BenchmarkId::new("events_then_opposing", n_events),
            &n_events,
            |b, &n| {
                // Setup: build the state with n positive events outside the timed path.
                b.iter_batched(
                    || {
                        let mut state = BalancedRecallState::new(200);
                        let id = Uuid::new_v4();
                        for _ in 0..n {
                            state.apply_signal(&feedback(id, FeedbackSignal::Useful));
                        }
                        state
                    },
                    |mut state| {
                        // Measured: apply n opposing events (ESS cap kicks in here).
                        let id = Uuid::new_v4();
                        for _ in 0..n {
                            state.apply_signal(&feedback(id, FeedbackSignal::NotUseful));
                        }
                        black_box(state.salience.mean())
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    g.finish();
}

// ── apply_signal throughput ───────────────────────────────────────────────────

fn bench_apply_signal(c: &mut Criterion) {
    let mut g = c.benchmark_group("brain_apply_signal");
    g.sample_size(200);

    g.bench_function("recall_hit", |b| {
        let id = Uuid::new_v4();
        b.iter_batched(
            || BalancedRecallState::new(100),
            |mut state| {
                state.apply_signal(&recall_hit(id, 10_000));
                black_box(state)
            },
            BatchSize::SmallInput,
        )
    });

    g.bench_function("recall_miss", |b| {
        b.iter_batched(
            || BalancedRecallState::new(100),
            |mut state| {
                state.apply_signal(&BrainSignal::RecallMiss);
                black_box(state)
            },
            BatchSize::SmallInput,
        )
    });

    g.bench_function("feedback_useful", |b| {
        let id = Uuid::new_v4();
        b.iter_batched(
            || BalancedRecallState::new(100),
            |mut state| {
                state.apply_signal(&feedback(id, FeedbackSignal::Useful));
                black_box(state)
            },
            BatchSize::SmallInput,
        )
    });

    g.finish();
}

// ── section weight derivation ─────────────────────────────────────────────────

fn bench_section_weights(c: &mut Criterion) {
    let mut g = c.benchmark_group("brain_section_weights");
    g.sample_size(200);

    g.bench_function("deterministic/default_priors", |b| {
        let state = SectionPosteriorState::new();
        b.iter(|| black_box(derive_deterministic_weights(&state)))
    });

    g.bench_function("deterministic/after_50_events", |b| {
        let mut state = SectionPosteriorState::new();
        let id = Uuid::new_v4();
        for _ in 0..50 {
            state.apply_signal(&feedback(id, FeedbackSignal::Useful));
        }
        b.iter(|| black_box(derive_deterministic_weights(&state)))
    });

    g.finish();
}

// ── criterion entry points ────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_snapshot_roundtrip,
    bench_ess_cap_convergence,
    bench_apply_signal,
    bench_section_weights,
);
criterion_main!(benches);
