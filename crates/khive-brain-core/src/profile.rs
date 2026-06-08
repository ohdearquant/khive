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

        // Semantic feedback: weighted posterior updates
        if let BrainSignal::SemanticFeedback {
            event_kind: ref ek, ..
        } = signal
        {
            let w = ek.update_weight();
            if ek.is_positive() {
                self.salience.update_success_weighted(w);
            } else {
                self.salience.update_failure_weighted(w);
            }
            if *ek == FeedbackEventKind::Correction {
                self.relevance.update_failure_weighted(w);
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
            ..
        } = signal
        {
            let posterior = self
                .entity_posteriors
                .get_or_insert(*eid, || BetaPosterior::new(1.0, 1.0));
            let w = ek.update_weight();
            if ek.is_positive() {
                posterior.update_success_weighted(w);
            } else {
                posterior.update_failure_weighted(w);
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancedRecallSnapshot {
    pub relevance: BetaPosterior,
    pub salience: BetaPosterior,
    pub temporal: BetaPosterior,
    pub entity_posteriors: HashMap<Uuid, BetaPosterior>,
    pub total_events: u64,
    pub exploration_epoch: u64,
}
