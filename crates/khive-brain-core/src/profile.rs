//! Profile lifecycle, records, bindings, and the BalancedRecall state.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::brain_signal::{entity_signal, is_recall_positive, BrainSignal};
use crate::posterior::{BetaPosterior, EntityPosteriors};
use crate::signal::{FeedbackEventKind, FeedbackSignal};

/// Lifecycle states for a registered profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileLifecycle {
    Defined,
    Registered,
    Active,
    Inactive,
    Archived,
}

/// Profile metadata stored in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileRecord {
    pub id: String,
    pub description: String,
    pub consumer_kind: String,
    pub state_class: String,
    pub lifecycle: ProfileLifecycle,
    pub created_at: DateTime<Utc>,
    pub state_snapshot: Option<serde_json::Value>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl ProfileRecord {
    pub fn new_balanced_recall(entity_capacity: usize) -> Self {
        let state = BalancedRecallState::new(entity_capacity);
        let snapshot = state.to_snapshot();
        Self {
            id: "balanced-recall-v1".into(),
            description: "Default recall profile: three-scalar Beta posteriors".into(),
            consumer_kind: "recall".into(),
            state_class: "Bayesian".into(),
            lifecycle: ProfileLifecycle::Active,
            created_at: Utc::now(),
            state_snapshot: serde_json::to_value(snapshot).ok(),
            total_events: 0,
            exploration_epoch: 0,
        }
    }
}

/// One row in the profile binding table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileBinding {
    pub actor: String,
    pub namespace: String,
    pub consumer_kind: String,
    pub profile_id: String,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
}

// ── BalancedRecallState ─────────────────────────────────────────────────────

/// Live Beta-posterior state for the `balanced-recall-v1` profile.
pub struct BalancedRecallState {
    pub relevance: BetaPosterior,
    pub salience: BetaPosterior,
    pub temporal: BetaPosterior,
    pub entity_posteriors: EntityPosteriors,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

impl BalancedRecallState {
    pub fn new(entity_capacity: usize) -> Self {
        Self {
            relevance: BetaPosterior::new(7.0, 3.0),
            salience: BetaPosterior::new(2.0, 8.0),
            temporal: BetaPosterior::new(1.0, 9.0),
            entity_posteriors: EntityPosteriors::new(entity_capacity),
            total_events: 0,
            exploration_epoch: 0,
        }
    }

    pub fn reset_posteriors(&mut self) {
        self.relevance = BetaPosterior::new(7.0, 3.0);
        self.salience = BetaPosterior::new(2.0, 8.0);
        self.temporal = BetaPosterior::new(1.0, 9.0);
        self.entity_posteriors.clear();
        self.exploration_epoch += 1;
    }

    pub fn to_snapshot(&self) -> BalancedRecallSnapshot {
        BalancedRecallSnapshot {
            relevance: self.relevance.clone(),
            salience: self.salience.clone(),
            temporal: self.temporal.clone(),
            entity_posteriors: self.entity_posteriors.to_snapshot(),
            entity_posteriors_version: 1,
            entity_posterior_order: self.entity_posteriors.order(),
            total_events: self.total_events,
            exploration_epoch: self.exploration_epoch,
        }
    }

    pub fn from_snapshot(snapshot: BalancedRecallSnapshot, entity_capacity: usize) -> Self {
        Self {
            relevance: snapshot.relevance,
            salience: snapshot.salience,
            temporal: snapshot.temporal,
            entity_posteriors: EntityPosteriors::from_snapshot(
                snapshot.entity_posteriors,
                snapshot.entity_posterior_order,
                entity_capacity,
            ),
            total_events: snapshot.total_events,
            exploration_epoch: snapshot.exploration_epoch,
        }
    }

    /// Apply a brain signal to update posteriors in place.
    /// This is the core profile update logic — pure, deterministic, no IO.
    pub fn apply_signal(&mut self, signal: &BrainSignal) {
        self.total_events += 1;

        // Global recall-relevance posterior
        if let Some(positive) = is_recall_positive(signal) {
            if positive {
                self.relevance.update_success();
            } else {
                self.relevance.update_failure();
            }
        }

        // Salience posterior — driven by explicit feedback signal
        if let BrainSignal::Feedback { signal: ref fb, .. } = signal {
            match fb {
                FeedbackSignal::Useful => self.salience.update_success(),
                FeedbackSignal::NotUseful | FeedbackSignal::Wrong => self.salience.update_failure(),
            }
        }

        // Semantic feedback: weighted posterior updates.
        //
        // ADR-081 §2: `effective_weight` can be `0.0` when the fold gate clamps
        // an over-cap implicit event. `update_*_weighted` requires a strictly
        // positive weight (it asserts), so a zero-weight event still counts
        // toward `total_events` (above) — it happened and was folded — but
        // contributes no posterior movement at all, matching "folded at zero
        // weight" literally rather than passing 0 into the weighted update.
        if let BrainSignal::SemanticFeedback {
            event_kind: ref ek,
            effective_weight,
            ..
        } = signal
        {
            let w = *effective_weight;
            if w > 0.0 {
                if ek.is_positive() {
                    self.salience.update_success_weighted(w);
                } else {
                    self.salience.update_failure_weighted(w);
                }
                if *ek == FeedbackEventKind::Correction {
                    self.relevance.update_failure_weighted(w);
                }
            }
        }

        // Temporal posterior — driven by recall latency
        const FAST_US: i64 = 50_000;
        match signal {
            BrainSignal::RecallHit { latency_us, .. } => {
                if *latency_us <= FAST_US {
                    self.temporal.update_success();
                } else {
                    self.temporal.update_failure();
                }
            }
            BrainSignal::RecallMiss => self.temporal.update_failure(),
            _ => {}
        }

        // Per-entity posterior updates
        if let BrainSignal::SemanticFeedback {
            target_id: eid,
            event_kind: ref ek,
            effective_weight,
            ..
        } = signal
        {
            let w = *effective_weight;
            if w > 0.0 {
                let posterior = self
                    .entity_posteriors
                    .get_or_insert(*eid, || BetaPosterior::new(1.0, 1.0));
                if ek.is_positive() {
                    posterior.update_success_weighted(w);
                } else {
                    posterior.update_failure_weighted(w);
                }
            }
        } else if let Some((entity_id, positive)) = entity_signal(signal) {
            let posterior = self
                .entity_posteriors
                .get_or_insert(entity_id, || BetaPosterior::new(1.0, 1.0));
            if positive {
                posterior.update_success();
            } else {
                posterior.update_failure();
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalancedRecallSnapshot {
    pub relevance: BetaPosterior,
    pub salience: BetaPosterior,
    pub temporal: BetaPosterior,
    pub entity_posteriors: HashMap<Uuid, BetaPosterior>,
    /// Snapshot schema version for `entity_posteriors`/`entity_posterior_order`.
    /// `0` (the serde default) marks a legacy snapshot with no order metadata;
    /// `1` marks a snapshot written with `entity_posterior_order` populated.
    #[serde(default)]
    pub entity_posteriors_version: u32,
    /// Eviction order (oldest first) for `entity_posteriors`, as of the write.
    /// Empty on legacy snapshots — restore falls back to a deterministic
    /// ascending-`Uuid` compatibility order in that case.
    #[serde(default)]
    pub entity_posterior_order: Vec<Uuid>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-048 Phase-1 gate: profile save/load round-trip must produce identical
    /// posteriors (snapshot == restored state).
    #[test]
    fn adr048_snapshot_roundtrip_equality() {
        let mut state = BalancedRecallState::new(100);
        let id = Uuid::new_v4();
        for _ in 0..50 {
            state.apply_signal(&crate::brain_signal::BrainSignal::RecallHit {
                target_id: id,
                latency_us: 10_000,
            });
        }
        let snap = state.to_snapshot();
        let restored = BalancedRecallState::from_snapshot(snap.clone(), 100);
        let restored_snap = restored.to_snapshot();

        // Full structural equality — covers every field including entity_posteriors.
        // If any field (including a newly-added one) is not preserved by the
        // round-trip, this assertion will catch it.
        assert_eq!(
            snap, restored_snap,
            "ADR-048 Phase-1: snapshot != restored state"
        );
    }

    /// ADR-048 Phase-1 gate: ESS cap convergence — after 200 positive events
    /// followed by 200 opposing events, the salience posterior mean must shift
    /// by at least 0.3.
    #[test]
    fn adr048_ess_cap_mean_shift_ge_0_3() {
        let mut state = BalancedRecallState::new(200);
        let id = Uuid::new_v4();

        for _ in 0..200 {
            state.apply_signal(&BrainSignal::Feedback {
                target_id: id,
                signal: FeedbackSignal::Useful,
                served_by_profile_id: None,
                section_signals: None,
            });
        }
        let mean_after_positive = state.salience.mean();

        for _ in 0..200 {
            state.apply_signal(&BrainSignal::Feedback {
                target_id: id,
                signal: FeedbackSignal::NotUseful,
                served_by_profile_id: None,
                section_signals: None,
            });
        }
        let mean_after_opposing = state.salience.mean();

        let shift = (mean_after_positive - mean_after_opposing).abs();
        assert!(
            shift >= 0.3,
            "ESS cap convergence: mean shift {shift:.4} < 0.3 (positive={mean_after_positive:.4}, opposing={mean_after_opposing:.4})"
        );
    }

    /// BRAINCORE-AUD-001 regression: a legacy snapshot (version 0, no order
    /// metadata) with more entries than the configured capacity must restore
    /// bounded, using a deterministic ascending-`Uuid` compatibility order.
    #[test]
    fn legacy_entity_posteriors_restore_uses_deterministic_order_and_capacity() {
        let capacity = 2;
        let mut ids = vec![
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        ];
        ids.sort();

        let mut entity_posteriors = HashMap::new();
        for id in &ids {
            entity_posteriors.insert(*id, BetaPosterior::default());
        }

        let legacy = BalancedRecallSnapshot {
            relevance: BetaPosterior::default(),
            salience: BetaPosterior::default(),
            temporal: BetaPosterior::default(),
            entity_posteriors,
            entity_posteriors_version: 0,
            entity_posterior_order: Vec::new(),
            total_events: 0,
            exploration_epoch: 0,
        };

        let restored = BalancedRecallState::from_snapshot(legacy, capacity);

        assert_eq!(restored.entity_posteriors.len(), capacity);
        for id in ids.iter().take(capacity) {
            assert!(
                restored.entity_posteriors.get(id).is_some(),
                "deterministic sort prefix id {id} must be retained"
            );
        }
        for id in ids.iter().skip(capacity) {
            assert!(
                restored.entity_posteriors.get(id).is_none(),
                "id {id} beyond the deterministic sort prefix must be dropped"
            );
        }
    }
}
