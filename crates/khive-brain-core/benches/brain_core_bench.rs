//! Criterion benchmarks for khive-brain-core primitives.
//!
//! These are micro-benchmarks of the core data structures.  They measure raw
//! primitive throughput and are intentionally isolated from the pack dispatch
//! path.  Do NOT cite these numbers as representative of the `brain.*`
//! verb API cost — that benchmark lives with the pack that dispatches those verbs.
//!
//! ADR-048 §"Benchmarks required" gates covered here:
//!   1. Profile save/load round-trip timing (BalancedRecallState).
//!   2. ESS cap convergence via SectionPosteriorState::apply_signal with
//!      section_signals (the actual capped path): 200 positive events then
//!      200 opposing; posterior mean for OperationalGuidance must shift ≥ 0.3.
//!
//! Run with:
//!   cd crates && cargo bench -p khive-brain-core --bench brain_core_bench

use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use khive_brain_core::{
    derive_deterministic_weights, BalancedRecallState, BrainSignal, BrainState, FeedbackSignal,
    SectionPosteriorState, SectionType,
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
    let mut g = c.benchmark_group("core_primitives/snapshot");
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

    // Bench 4: full BrainState round-trip — both to_snapshot AND from_snapshot
    // are inside the timed closure so the reported number reflects the real
    // serialise-then-restore cost, not restore-only.
    g.bench_function("brain_state/roundtrip", |b| {
        let mut brain = BrainState::new(100);
        let id = Uuid::new_v4();
        for _ in 0..50 {
            brain.balanced_recall.apply_signal(&recall_hit(id, 20_000));
        }
        b.iter(|| {
            let snap = black_box(&brain).to_snapshot();
            black_box(BrainState::from_snapshot(snap, 100))
        })
    });

    g.finish();
}

// ── ESS cap convergence ───────────────────────────────────────────────────────
//
// Drives SectionPosteriorState::apply_signal — the path that actually calls
// BetaPosterior::apply_ess_cap.  BalancedRecallState::apply_signal does NOT
// call apply_ess_cap; using it here was the methodology defect fixed in this
// commit.
//
// Correctness gate (outside the timed closure): after 200 positive then 200
// opposing section feedback events for OperationalGuidance, the posterior mean
// for that section must shift by ≥ 0.3.  This assertion runs in setup so that
// a broken apply_ess_cap path causes an immediate bench failure rather than
// silently producing misleading timing numbers.

fn section_signal(id: Uuid, st: SectionType, signal: FeedbackSignal) -> BrainSignal {
    use std::collections::HashMap;
    let mut section_signals = HashMap::new();
    section_signals.insert(st, signal);
    BrainSignal::Feedback {
        target_id: id,
        signal: FeedbackSignal::Useful,
        served_by_profile_id: None,
        section_signals: Some(section_signals),
    }
}

fn bench_ess_cap_convergence(c: &mut Criterion) {
    let mut g = c.benchmark_group("core_primitives/ess_cap");
    g.sample_size(100);

    // ── Correctness assertion (not timed) ────────────────────────────────────
    // Verify that apply_ess_cap is actually reached and produces the required
    // mean shift.  A failure here means the capped path is broken, not slow.
    {
        let id = Uuid::new_v4();
        let mut state = SectionPosteriorState::new();
        let mean_before = state
            .posteriors
            .get(&SectionType::OperationalGuidance)
            .map(|p| p.mean())
            .unwrap_or(0.5);
        for _ in 0..200 {
            state.apply_signal(&section_signal(
                id,
                SectionType::OperationalGuidance,
                FeedbackSignal::Useful,
            ));
        }
        let mean_after_positive = state
            .posteriors
            .get(&SectionType::OperationalGuidance)
            .map(|p| p.mean())
            .unwrap_or(0.5);
        for _ in 0..200 {
            state.apply_signal(&section_signal(
                id,
                SectionType::OperationalGuidance,
                FeedbackSignal::NotUseful,
            ));
        }
        let mean_after_opposing = state
            .posteriors
            .get(&SectionType::OperationalGuidance)
            .map(|p| p.mean())
            .unwrap_or(0.5);
        let shift = (mean_after_positive - mean_after_opposing).abs();
        assert!(
            shift >= 0.3,
            "ESS cap correctness gate: OperationalGuidance mean shift {shift:.4} < 0.3 \
             (before={mean_before:.4}, after_pos={mean_after_positive:.4}, \
             after_opp={mean_after_opposing:.4})"
        );
    }

    // ── Timing: apply n opposing section-feedback events on the capped path ─
    for &n_events in &[50usize, 100, 200] {
        g.bench_with_input(
            BenchmarkId::new("section_posterior_capped", n_events),
            &n_events,
            |b, &n| {
                b.iter_batched(
                    || {
                        // Setup: prime the state with n positive section events.
                        let id = Uuid::new_v4();
                        let mut state = SectionPosteriorState::new();
                        for _ in 0..n {
                            state.apply_signal(&section_signal(
                                id,
                                SectionType::OperationalGuidance,
                                FeedbackSignal::Useful,
                            ));
                        }
                        (state, id)
                    },
                    |(mut state, id)| {
                        // Measured: apply n opposing events.  ESS cap fires on
                        // each call because the accumulated pseudo-count exceeds
                        // DEFAULT_ESS_CAP (100.0) after the setup phase.
                        for _ in 0..n {
                            state.apply_signal(&section_signal(
                                id,
                                SectionType::OperationalGuidance,
                                FeedbackSignal::NotUseful,
                            ));
                        }
                        black_box(
                            state
                                .posteriors
                                .get(&SectionType::OperationalGuidance)
                                .map(|p| p.mean()),
                        )
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
    let mut g = c.benchmark_group("core_primitives/apply_signal");
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
    let mut g = c.benchmark_group("core_primitives/section_weights");
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
