//! Profile lifecycle, records, bindings, and the BalancedRecall state.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

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

/// Closed vocabulary of brain profile consumers (ADR-058 amendment, #542).
/// Adding a new consumer requires adding a variant here — never a bare string
/// literal at a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerKind {
    Recall,
    KnowledgeCompose,
    Rerank,
}

impl ConsumerKind {
    /// The exact wire-level `consumer_kind` string for this variant.
    pub const fn as_str(&self) -> &'static str {
        match self {
            ConsumerKind::Recall => "recall",
            ConsumerKind::KnowledgeCompose => "knowledge_compose",
            ConsumerKind::Rerank => "rerank",
        }
    }
}

impl fmt::Display for ConsumerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ConsumerKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "recall" => Ok(ConsumerKind::Recall),
            "knowledge_compose" => Ok(ConsumerKind::KnowledgeCompose),
            "rerank" => Ok(ConsumerKind::Rerank),
            other => Err(format!("unknown ConsumerKind: {other:?}")),
        }
    }
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
    /// Count of `brain.reset` invocations against this profile. Despite the
    /// name, no exploration schedule exists — nothing else increments it, and
    /// it stays 0 for profiles that have never been reset. Renaming to
    /// `reset_epoch` is a versioned output-shape change deferred to the
    /// posterior-serving lane (ADR-104 follow-up).
    pub exploration_epoch: u64,
}

impl ProfileRecord {
    pub fn new_balanced_recall(entity_capacity: usize) -> Self {
        let state = BalancedRecallState::new(entity_capacity);
        let snapshot = state.to_snapshot();
        Self {
            id: "balanced-recall-v1".into(),
            description: "Default recall profile: three-scalar Beta posteriors".into(),
            consumer_kind: ConsumerKind::Recall.as_str().into(),
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

/// Resolve the effective profile for an `(actor, namespace, consumer_kind)`
/// triple via the brain pack's tier-2 binding table. Returns `None` unless
/// `brain.resolve` reports `matched_binding=true` — the system-default
/// fallback profile it returns otherwise must not be mistaken for an explicit
/// binding, or each pack's tier-3 (global tuning prior) would become
/// unreachable.
///
/// The authorized `token` supplies both the actor used for binding matching
/// and the per-request identity used for the nested dispatch. Anonymous
/// callers map to `actor: null`; their raw `id` ("local") must never match an
/// explicit `actor="local"` binding. Reusing the token identity also prevents
/// a warm registry's construction-baked principal from replacing the outer
/// caller at the nested Gate boundary (ADR-096).
///
/// Shared by the memory pack (`ConsumerKind::Recall`) and the knowledge pack
/// (`ConsumerKind::KnowledgeCompose`) — ADR-058 amendment, #542; actor-aware
/// resolution added by #697.
pub async fn resolve_consumer_profile(
    registry: &khive_runtime::VerbRegistry,
    token: &khive_runtime::NamespaceToken,
    consumer_kind: ConsumerKind,
) -> Option<String> {
    // Without the brain pack loaded there is no binding table to consult;
    // skip the dispatch entirely rather than paying a guaranteed-failed
    // call (and its audit write) on every consumer invocation.
    if !registry.has_verb("brain.resolve") {
        return None;
    }
    let actor = token.actor().binding_id();
    let namespace = token.namespace().as_str();
    let resolve_params = serde_json::json!({
        "actor": actor,
        "namespace": namespace,
        "consumer_kind": consumer_kind.as_str(),
    });
    match registry
        .dispatch_with_identity(
            "brain.resolve",
            resolve_params,
            Some(khive_runtime::RequestIdentity::from_token(token)),
        )
        .await
    {
        Ok(v) => {
            let matched_binding = v
                .get("matched_binding")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if matched_binding {
                v.get("resolved_profile_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_owned)
            } else {
                None
            }
        }
        Err(_) => None,
    }
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

        self.apply_feedback_posteriors(signal);

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

        if matches!(
            signal,
            BrainSignal::Feedback { .. } | BrainSignal::SemanticFeedback { .. }
        ) {
            return;
        }
        if let Some((entity_id, positive)) = entity_signal(signal) {
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

    /// Update feedback posteriors without changing counters or exploration state.
    /// Non-feedback signals and nonpositive semantic weights leave them untouched.
    pub fn apply_feedback_posteriors(&mut self, signal: &BrainSignal) {
        match signal {
            BrainSignal::Feedback {
                target_id, signal, ..
            } => {
                let positive = matches!(signal, FeedbackSignal::Useful);
                if positive {
                    self.salience.update_success();
                } else {
                    self.salience.update_failure();
                }
                let posterior = self
                    .entity_posteriors
                    .get_or_insert(*target_id, || BetaPosterior::new(1.0, 1.0));
                if positive {
                    posterior.update_success();
                } else {
                    posterior.update_failure();
                }
            }
            BrainSignal::SemanticFeedback {
                target_id,
                event_kind,
                effective_weight,
                ..
            } if *effective_weight > 0.0 => {
                let weight = *effective_weight;
                if event_kind.is_positive() {
                    self.salience.update_success_weighted(weight);
                } else {
                    self.salience.update_failure_weighted(weight);
                }
                if *event_kind == FeedbackEventKind::Correction {
                    self.relevance.update_failure_weighted(weight);
                }
                let posterior = self
                    .entity_posteriors
                    .get_or_insert(*target_id, || BetaPosterior::new(1.0, 1.0));
                if event_kind.is_positive() {
                    posterior.update_success_weighted(weight);
                } else {
                    posterior.update_failure_weighted(weight);
                }
            }
            _ => {}
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
#[path = "profile_tests.rs"]
mod tests;
