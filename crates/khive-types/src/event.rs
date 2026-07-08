//! Event substrate — append-only log produced by every verb execution.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::{Header, Id128, SubstrateKind};

/// A system event. Append-only, never mutated or deleted.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Event {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub header: Header,
    /// The verb that produced the event.
    pub verb: String,
    /// Which substrate type was acted upon.
    pub substrate: SubstrateKind,
    /// Who performed the action. Profile- or system-produced events may omit it.
    pub actor: Option<String>,
    /// Typed event discriminant used by replay, projections, and workers.
    pub kind: EventKind,
    /// Typed payload surface for known event families; raw JSON is still allowed.
    pub payload: EventPayload,
    /// Payload schema version interpreted per `kind`.
    pub payload_schema_version: u32,
    /// Brain profile state version observed when the event was emitted.
    pub profile_state_version: Option<u64>,
    /// Logical aggregate threaded across related event ids.
    pub aggregate: Option<AggregateRef>,
}

/// Outcome of a verb execution recorded in an event log entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EventOutcome {
    /// The verb executed successfully.
    #[default]
    Success,
    /// The verb was denied by a policy check.
    Denied,
    /// The verb encountered a runtime error.
    Error,
}

impl EventOutcome {
    /// Return the canonical lowercase string for this outcome.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Denied => "denied",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for EventOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Discriminant for the 26 typed event variants produced by the verb dispatch path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EventKind {
    /// Generic audit event with no structured payload.
    Audit,
    /// A `recall` verb was executed and results were returned.
    RecallExecuted,
    /// A rerank pass was applied to search candidates.
    RerankExecuted,
    /// A `search` verb was executed.
    SearchExecuted,
    /// A new directed edge was created between two nodes.
    LinkCreated,
    /// A new entity was created.
    EntityCreated,
    /// An existing entity was patched.
    EntityUpdated,
    /// An entity was soft- or hard-deleted.
    EntityDeleted,
    /// Two entities were merged (deduplication).
    EntityMerged,
    /// A new note was created.
    NoteCreated,
    /// An existing note was patched.
    NoteUpdated,
    /// A note was soft- or hard-deleted.
    NoteDeleted,
    /// An edge's relation or weight was updated.
    EdgeUpdated,
    /// An edge was removed.
    EdgeDeleted,
    /// A GTD task moved between lifecycle states.
    TaskTransitioned,
    /// An explicit user feedback signal was recorded.
    FeedbackExplicit,
    /// The brain recommended a profile resolution update.
    ProfileResolutionRecommended,
    /// Two brain profiles were merged.
    ProfileMerged,
    /// The active embedding model was changed.
    EmbeddingModelChanged,
    /// An embedding migration batch completed successfully.
    EmbeddingMigrationCompleted,
    /// An embedding migration batch failed.
    EmbeddingMigrationFailed,
    /// Drift was detected between stored and live embeddings.
    EmbeddingDriftDetected,
    /// A proposal was submitted for review.
    ProposalCreated,
    /// A reviewer accepted, rejected, or commented on a proposal.
    ProposalReviewed,
    /// A proposal was applied to the graph.
    ProposalApplied,
    /// A proposal was withdrawn before it was applied.
    ProposalWithdrawn,
    /// A channel poll cycle started for one `(kind, slug)` credential.
    ChannelPollStarted,
    /// A channel poll cycle returned envelopes after a prior failure.
    ChannelPollSucceeded,
    /// A channel poll cycle failed.
    ChannelPollFailed,
    /// A channel's backoff escalated to a new step after a failure.
    ChannelBackoffArmed,
    /// A channel's backoff reset to base after a success.
    ChannelBackoffReset,
    /// Persisting a channel heartbeat row failed.
    ChannelHeartbeatPersistFailed,
    /// A process-lifetime `OnceLock` configuration value was locked in.
    ConfigLocked,
    /// A WAL checkpoint tick's outcome was recorded (ADR-091 elevated/drain edge).
    CheckpointOutcomeRecorded,
}

impl EventKind {
    /// All 34 event kind variants in declaration order.
    pub const ALL: [Self; 34] = [
        Self::Audit,
        Self::RecallExecuted,
        Self::RerankExecuted,
        Self::SearchExecuted,
        Self::LinkCreated,
        Self::EntityCreated,
        Self::EntityUpdated,
        Self::EntityDeleted,
        Self::EntityMerged,
        Self::NoteCreated,
        Self::NoteUpdated,
        Self::NoteDeleted,
        Self::EdgeUpdated,
        Self::EdgeDeleted,
        Self::TaskTransitioned,
        Self::FeedbackExplicit,
        Self::ProfileResolutionRecommended,
        Self::ProfileMerged,
        Self::EmbeddingModelChanged,
        Self::EmbeddingMigrationCompleted,
        Self::EmbeddingMigrationFailed,
        Self::EmbeddingDriftDetected,
        Self::ProposalCreated,
        Self::ProposalReviewed,
        Self::ProposalApplied,
        Self::ProposalWithdrawn,
        Self::ChannelPollStarted,
        Self::ChannelPollSucceeded,
        Self::ChannelPollFailed,
        Self::ChannelBackoffArmed,
        Self::ChannelBackoffReset,
        Self::ChannelHeartbeatPersistFailed,
        Self::ConfigLocked,
        Self::CheckpointOutcomeRecorded,
    ];

    /// Return the canonical snake_case string for this event kind.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Audit => "audit",
            Self::RecallExecuted => "recall_executed",
            Self::RerankExecuted => "rerank_executed",
            Self::SearchExecuted => "search_executed",
            Self::LinkCreated => "link_created",
            Self::EntityCreated => "entity_created",
            Self::EntityUpdated => "entity_updated",
            Self::EntityDeleted => "entity_deleted",
            Self::EntityMerged => "entity_merged",
            Self::NoteCreated => "note_created",
            Self::NoteUpdated => "note_updated",
            Self::NoteDeleted => "note_deleted",
            Self::EdgeUpdated => "edge_updated",
            Self::EdgeDeleted => "edge_deleted",
            Self::TaskTransitioned => "task_transitioned",
            Self::FeedbackExplicit => "feedback_explicit",
            Self::ProfileResolutionRecommended => "profile_resolution_recommended",
            Self::ProfileMerged => "profile_merged",
            Self::EmbeddingModelChanged => "embedding_model_changed",
            Self::EmbeddingMigrationCompleted => "embedding_migration_completed",
            Self::EmbeddingMigrationFailed => "embedding_migration_failed",
            Self::EmbeddingDriftDetected => "embedding_drift_detected",
            Self::ProposalCreated => "proposal_created",
            Self::ProposalReviewed => "proposal_reviewed",
            Self::ProposalApplied => "proposal_applied",
            Self::ProposalWithdrawn => "proposal_withdrawn",
            Self::ChannelPollStarted => "channel_poll_started",
            Self::ChannelPollSucceeded => "channel_poll_succeeded",
            Self::ChannelPollFailed => "channel_poll_failed",
            Self::ChannelBackoffArmed => "channel_backoff_armed",
            Self::ChannelBackoffReset => "channel_backoff_reset",
            Self::ChannelHeartbeatPersistFailed => "channel_heartbeat_persist_failed",
            Self::ConfigLocked => "config_locked",
            Self::CheckpointOutcomeRecorded => "checkpoint_outcome_recorded",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

const EVENT_KIND_VALID: &[&str] = &[
    "audit",
    "recall_executed",
    "rerank_executed",
    "search_executed",
    "link_created",
    "entity_created",
    "entity_updated",
    "entity_deleted",
    "entity_merged",
    "note_created",
    "note_updated",
    "note_deleted",
    "edge_updated",
    "edge_deleted",
    "task_transitioned",
    "feedback_explicit",
    "profile_resolution_recommended",
    "profile_merged",
    "embedding_model_changed",
    "embedding_migration_completed",
    "embedding_migration_failed",
    "embedding_drift_detected",
    "proposal_created",
    "proposal_reviewed",
    "proposal_applied",
    "proposal_withdrawn",
    "channel_poll_started",
    "channel_poll_succeeded",
    "channel_poll_failed",
    "channel_backoff_armed",
    "channel_backoff_reset",
    "channel_heartbeat_persist_failed",
    "config_locked",
    "checkpoint_outcome_recorded",
];

impl core::str::FromStr for EventKind {
    type Err = crate::error::UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "audit" => Ok(Self::Audit),
            "recall_executed" => Ok(Self::RecallExecuted),
            "rerank_executed" => Ok(Self::RerankExecuted),
            "search_executed" => Ok(Self::SearchExecuted),
            "link_created" => Ok(Self::LinkCreated),
            "entity_created" => Ok(Self::EntityCreated),
            "entity_updated" => Ok(Self::EntityUpdated),
            "entity_deleted" => Ok(Self::EntityDeleted),
            "entity_merged" => Ok(Self::EntityMerged),
            "note_created" => Ok(Self::NoteCreated),
            "note_updated" => Ok(Self::NoteUpdated),
            "note_deleted" => Ok(Self::NoteDeleted),
            "edge_updated" => Ok(Self::EdgeUpdated),
            "edge_deleted" => Ok(Self::EdgeDeleted),
            "task_transitioned" => Ok(Self::TaskTransitioned),
            "feedback_explicit" => Ok(Self::FeedbackExplicit),
            "profile_resolution_recommended" => Ok(Self::ProfileResolutionRecommended),
            "profile_merged" => Ok(Self::ProfileMerged),
            "embedding_model_changed" => Ok(Self::EmbeddingModelChanged),
            "embedding_migration_completed" => Ok(Self::EmbeddingMigrationCompleted),
            "embedding_migration_failed" => Ok(Self::EmbeddingMigrationFailed),
            "embedding_drift_detected" => Ok(Self::EmbeddingDriftDetected),
            "proposal_created" => Ok(Self::ProposalCreated),
            "proposal_reviewed" => Ok(Self::ProposalReviewed),
            "proposal_applied" => Ok(Self::ProposalApplied),
            "proposal_withdrawn" => Ok(Self::ProposalWithdrawn),
            "channel_poll_started" => Ok(Self::ChannelPollStarted),
            "channel_poll_succeeded" => Ok(Self::ChannelPollSucceeded),
            "channel_poll_failed" => Ok(Self::ChannelPollFailed),
            "channel_backoff_armed" => Ok(Self::ChannelBackoffArmed),
            "channel_backoff_reset" => Ok(Self::ChannelBackoffReset),
            "channel_heartbeat_persist_failed" => Ok(Self::ChannelHeartbeatPersistFailed),
            "config_locked" => Ok(Self::ConfigLocked),
            "checkpoint_outcome_recorded" => Ok(Self::CheckpointOutcomeRecorded),
            other => Err(crate::error::UnknownVariant::new(
                "event_kind",
                other,
                EVENT_KIND_VALID,
            )),
        }
    }
}

/// A reference to the logical aggregate that an event belongs to.
///
/// Used to thread related events (e.g. proposal lifecycle events) into a
/// single auditable chain identified by `kind` and `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AggregateRef {
    /// The aggregate type string (e.g. `"proposal"`).
    pub kind: String,
    /// The aggregate instance identifier.
    pub id: Id128,
}

/// Typed payload for an [`Event`], dispatched by [`EventKind`].
///
/// The `Json` variant is a catch-all for events whose payload has not yet
/// been promoted to a structured type. All other variants carry a concrete
/// typed struct that can be pattern-matched without round-tripping through JSON.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(tag = "kind", content = "payload", rename_all = "snake_case")
)]
pub enum EventPayload {
    /// Raw JSON payload for untyped events.
    Json(String),
    /// Structured payload for a rerank pass event.
    RerankExecuted(RerankExecutedPayload),
    /// Structured payload for a proposal-created event (requires `serde` feature).
    #[cfg(feature = "serde")]
    ProposalCreated(ProposalCreatedPayload),
    /// Structured payload for a proposal-reviewed event.
    ProposalReviewed(ProposalReviewedPayload),
    /// Structured payload for a proposal-applied event.
    ProposalApplied(ProposalAppliedPayload),
    /// Structured payload for a proposal-withdrawn event.
    ProposalWithdrawn(ProposalWithdrawnPayload),
}

impl Default for EventPayload {
    fn default() -> Self {
        Self::Json("{}".into())
    }
}

/// Payload for a rerank pass event, recording per-candidate scores.
///
/// All score values (`reranked` section scores, `final_scores`) must be finite.
/// When the `serde` feature is enabled, deserialization rejects non-finite scores.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct RerankExecutedPayload {
    /// Brain profile that served this rerank, if any.
    pub served_by_profile_id: Option<String>,
    /// Model used for reranking.
    pub model_id: Id128,
    /// Candidate IDs in input order.
    pub candidates: Vec<Id128>,
    /// Per-candidate named sub-scores from the reranker.
    pub reranked: Vec<(Id128, Vec<(String, f32)>)>,
    /// Final aggregated score per candidate.
    pub final_scores: Vec<(Id128, f32)>,
    /// Wall-clock latency of the rerank operation in microseconds.
    pub latency_us: u64,
    /// Whether a brain hook was applied during this rerank.
    pub hook_applied: bool,
    /// Whether the hook matched the intended target.
    pub hook_target_match: bool,
}

impl RerankExecutedPayload {
    /// Return `true` if all score values are finite.
    pub fn is_valid(&self) -> bool {
        let reranked_ok = self
            .reranked
            .iter()
            .all(|(_, scores)| scores.iter().all(|(_, s)| s.is_finite()));
        let final_ok = self.final_scores.iter().all(|(_, s)| s.is_finite());
        reranked_ok && final_ok
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for RerankExecutedPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        struct Raw {
            served_by_profile_id: Option<String>,
            model_id: Id128,
            candidates: Vec<Id128>,
            reranked: Vec<(Id128, Vec<(String, f32)>)>,
            final_scores: Vec<(Id128, f32)>,
            latency_us: u64,
            hook_applied: bool,
            hook_target_match: bool,
        }

        let raw = Raw::deserialize(deserializer)?;

        for (_, score) in &raw.final_scores {
            if !score.is_finite() {
                return Err(serde::de::Error::custom(alloc::format!(
                    "RerankExecutedPayload final_scores must be finite, got {score}"
                )));
            }
        }
        for (_, sections) in &raw.reranked {
            for (section_name, score) in sections {
                if !score.is_finite() {
                    return Err(serde::de::Error::custom(alloc::format!(
                        "RerankExecutedPayload reranked section '{section_name}' score must be finite, got {score}"
                    )));
                }
            }
        }

        Ok(RerankExecutedPayload {
            served_by_profile_id: raw.served_by_profile_id,
            model_id: raw.model_id,
            candidates: raw.candidates,
            reranked: raw.reranked,
            final_scores: raw.final_scores,
            latency_us: raw.latency_us,
            hook_applied: raw.hook_applied,
            hook_target_match: raw.hook_target_match,
        })
    }
}

/// Payload for the `ProposalCreated` event — captures the full initial proposal state.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProposalCreatedPayload {
    pub proposal_id: Id128,
    pub proposer: String,
    pub title: String,
    pub description: String,
    pub changeset: ProposalChangeset,
    pub reviewers: Vec<String>,
    pub expiry: Option<crate::Timestamp>,
    pub parent_id: Option<Id128>,
}

/// Structured draft for adding a new entity via a proposal.
///
/// Fields mirror the `create(kind=<entity kind>)` verb surface; `kind` is
/// validated against the closed 8-kind entity taxonomy at apply time.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityDraft {
    /// Entity kind — must be one of the 8 closed entity kind values.
    pub kind: String,
    /// Human-readable name (required).
    pub name: String,
    /// Optional long-form description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Arbitrary structured metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    /// Classification tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Structured patch for modifying an existing entity via a proposal.
///
/// Absent fields mean "leave unchanged". Setting `description` to `null` clears it.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProposalEntityPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `null` clears the description; absent leaves it unchanged.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "serde_opt_opt"
    )]
    pub description: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Structured draft for adding a new note via a proposal.
///
/// Fields mirror the `create(kind=<note kind>)` verb surface.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NoteDraft {
    /// Note kind string (validated by the loaded pack at apply time).
    pub kind: String,
    /// Note body / content (required).
    pub content: String,
    /// Optional short name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Arbitrary structured metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
}

/// Serde helper for `Option<Option<T>>` — distinguishes absent vs. explicit null.
#[cfg(feature = "serde")]
mod serde_opt_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(val: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match val {
            None => unreachable!("skip_serializing_if guards the None case"),
            Some(inner) => inner.serialize(s),
        }
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let opt: Option<T> = Option::deserialize(d)?;
        Ok(Some(opt))
    }
}

/// The set of KG mutations a proposal intends to apply as a proposal changeset.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposalChangeset {
    /// Add a new entity. `entity.kind` validated at apply time.
    AddEntity {
        entity: EntityDraft,
    },
    /// Modify an existing entity's properties / tags / description.
    UpdateEntity {
        id: Id128,
        patch: ProposalEntityPatch,
    },
    /// Add a typed edge. `weight` must be finite and in `[0.0, 1.0]` if present.
    AddEdge {
        source: Id128,
        target: Id128,
        relation: crate::EdgeRelation,
        weight: Option<f32>,
    },
    /// Add a note (entity-annotating or stand-alone).
    AddNote {
        note: NoteDraft,
    },
    MergeEntities {
        into: Id128,
        from: Id128,
    },
    SupersedeEntity {
        old: Id128,
        new: Id128,
    },
    Compound {
        steps: Vec<ProposalChangeset>,
    },
}

#[cfg(feature = "serde")]
impl ProposalChangeset {
    fn validate(&self) -> Result<(), alloc::string::String> {
        match self {
            Self::AddEdge { weight, .. } => {
                if let Some(w) = weight {
                    if !w.is_finite() {
                        return Err(alloc::format!(
                            "ProposalChangeset AddEdge weight must be finite, got {w}"
                        ));
                    }
                    if !(*w >= 0.0 && *w <= 1.0) {
                        return Err(alloc::format!(
                            "ProposalChangeset AddEdge weight must be in [0.0, 1.0], got {w}"
                        ));
                    }
                }
                Ok(())
            }
            Self::Compound { steps } => {
                for step in steps {
                    step.validate()?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ProposalChangeset {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(serde::Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum ProposalChangesetRaw {
            AddEntity {
                entity: EntityDraft,
            },
            UpdateEntity {
                id: Id128,
                patch: ProposalEntityPatch,
            },
            AddEdge {
                source: Id128,
                target: Id128,
                relation: crate::EdgeRelation,
                weight: Option<f32>,
            },
            AddNote {
                note: NoteDraft,
            },
            MergeEntities {
                into: Id128,
                from: Id128,
            },
            SupersedeEntity {
                old: Id128,
                new: Id128,
            },
            Compound {
                steps: Vec<ProposalChangeset>,
            },
        }

        let raw = ProposalChangesetRaw::deserialize(deserializer)?;
        let cs = match raw {
            ProposalChangesetRaw::AddEntity { entity } => Self::AddEntity { entity },
            ProposalChangesetRaw::UpdateEntity { id, patch } => Self::UpdateEntity { id, patch },
            ProposalChangesetRaw::AddEdge {
                source,
                target,
                relation,
                weight,
            } => Self::AddEdge {
                source,
                target,
                relation,
                weight,
            },
            ProposalChangesetRaw::AddNote { note } => Self::AddNote { note },
            ProposalChangesetRaw::MergeEntities { into, from } => {
                Self::MergeEntities { into, from }
            }
            ProposalChangesetRaw::SupersedeEntity { old, new } => {
                Self::SupersedeEntity { old, new }
            }
            ProposalChangesetRaw::Compound { steps } => Self::Compound { steps },
        };
        cs.validate().map_err(serde::de::Error::custom)?;
        Ok(cs)
    }
}

#[cfg(not(feature = "serde"))]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalChangeset {
    AddEdge {
        source: Id128,
        target: Id128,
        relation: crate::EdgeRelation,
        weight: Option<f32>,
    },
    MergeEntities {
        into: Id128,
        from: Id128,
    },
    SupersedeEntity {
        old: Id128,
        new: Id128,
    },
    Compound {
        steps: Vec<ProposalChangeset>,
    },
}

/// Payload for the `ProposalReviewed` event — records a single reviewer's decision.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProposalReviewedPayload {
    pub proposal_id: Id128,
    pub reviewer: String,
    pub decision: ProposalDecision,
    pub comment: Option<String>,
}

/// A reviewer's decision on a proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProposalDecision {
    /// The reviewer approved the proposal for application.
    Approve,
    /// The reviewer rejected the proposal; it will not be applied.
    Reject,
    /// The reviewer left a comment without blocking the proposal.
    Comment,
    /// The reviewer requested changes before the proposal can proceed.
    RequestChanges,
}

impl ProposalDecision {
    /// Returns the bare variant name as a lowercase string, matching the serde
    /// `rename_all = "snake_case"` representation.  Use this when storing the
    /// decision as a plain TEXT column — **not** `serde_json::to_string`, which
    /// would produce a JSON-quoted string (`"\"approve\""` instead of `"approve"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::Comment => "comment",
            Self::RequestChanges => "request_changes",
        }
    }
}

/// Payload for the `ProposalApplied` event — records the outcome of the apply attempt.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProposalAppliedPayload {
    pub proposal_id: Id128,
    pub applied_at: crate::Timestamp,
    pub applied_by: String,
    pub result: ApplyResult,
}

/// Outcome of applying a proposal: either all steps succeeded or the apply failed with an error.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ApplyResult {
    Success {
        created_records: Vec<Id128>,
    },
    Failed {
        error: String,
        applied_step_count: u32,
    },
}

/// Payload for the `ProposalWithdrawn` event — records who withdrew and an optional reason.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProposalWithdrawnPayload {
    pub proposal_id: Id128,
    pub by: String,
    pub reason: Option<String>,
}

/// Builder for events. Used by the verb dispatch path.
pub struct EventBuilder {
    verb: String,
    substrate: SubstrateKind,
    actor: Option<String>,
    kind: EventKind,
    payload: EventPayload,
    payload_schema_version: u32,
    profile_state_version: Option<u64>,
    aggregate: Option<AggregateRef>,
}

impl EventBuilder {
    /// Create a new builder for an event produced by `verb` acting on `substrate` as `actor`.
    pub fn new(
        verb: impl Into<String>,
        substrate: SubstrateKind,
        actor: impl Into<String>,
    ) -> Self {
        Self {
            verb: verb.into(),
            substrate,
            actor: Some(actor.into()),
            kind: EventKind::Audit,
            payload: EventPayload::default(),
            payload_schema_version: 1,
            profile_state_version: None,
            aggregate: None,
        }
    }

    /// Override the event kind discriminant.
    pub fn kind(mut self, kind: EventKind) -> Self {
        self.kind = kind;
        self
    }

    /// Set the typed payload for this event.
    pub fn payload(mut self, payload: EventPayload) -> Self {
        self.payload = payload;
        self
    }

    /// Set the payload schema version (defaults to 1).
    pub fn payload_schema_version(mut self, version: u32) -> Self {
        self.payload_schema_version = version;
        self
    }

    /// Record the brain profile state version observed at emit time.
    pub fn profile_state_version(mut self, version: u64) -> Self {
        self.profile_state_version = Some(version);
        self
    }

    /// Thread this event into an aggregate chain.
    pub fn aggregate(mut self, aggregate: AggregateRef) -> Self {
        self.aggregate = Some(aggregate);
        self
    }

    /// Consume the builder and produce an [`Event`] with the given `header`.
    pub fn build(self, header: Header) -> Event {
        Event {
            header,
            verb: self.verb,
            substrate: self.substrate,
            actor: self.actor,
            kind: self.kind,
            payload: self.payload,
            payload_schema_version: self.payload_schema_version,
            profile_state_version: self.profile_state_version,
            aggregate: self.aggregate,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use crate::{Namespace, Timestamp};
    #[cfg(feature = "serde")]
    use alloc::string::ToString;

    fn header() -> Header {
        Header::new(
            Id128::from_u128(1),
            Namespace::local(),
            Timestamp::from_secs(1700000000),
        )
    }

    #[test]
    fn event_kind_parse_roundtrip() {
        for kind in EventKind::ALL {
            let parsed: EventKind = kind
                .name()
                .parse()
                .expect("EventKind::name must parse back");
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn rerank_payload_records_served_profile() {
        let payload = EventPayload::RerankExecuted(RerankExecutedPayload {
            served_by_profile_id: Some("profile-a".into()),
            model_id: Id128::from_u128(1),
            candidates: Vec::new(),
            reranked: Vec::new(),
            final_scores: Vec::new(),
            latency_us: 100,
            hook_applied: false,
            hook_target_match: false,
        });
        let event = EventBuilder::new("rerank", SubstrateKind::Note, "agent:test")
            .kind(EventKind::RerankExecuted)
            .payload(payload)
            .build(header());

        if let EventPayload::RerankExecuted(ref p) = event.payload {
            assert_eq!(p.served_by_profile_id.as_deref(), Some("profile-a"));
        } else {
            panic!("unexpected payload variant");
        }
    }

    #[test]
    fn proposal_payloads_are_typed() {
        let payload = EventPayload::ProposalReviewed(ProposalReviewedPayload {
            proposal_id: Id128::from_u128(42),
            reviewer: "operator".into(),
            decision: ProposalDecision::Approve,
            comment: None,
        });
        let event = EventBuilder::new("review", SubstrateKind::Entity, "operator")
            .kind(EventKind::ProposalReviewed)
            .payload(payload)
            .build(header());
        assert_eq!(event.kind.name(), "proposal_reviewed");
    }

    /// C1 regression: all ProposalChangeset variants that carry Id128 fields must
    /// round-trip through serde_json::Value.  Previously `Id128::deserialize` used
    /// `<&str>::deserialize` which fails when the deserializer holds owned data
    /// (the Value-backed path used by the MCP DSL parser).
    #[cfg(feature = "serde")]
    #[test]
    fn proposal_changeset_id_variants_deserialize_from_value() {
        let uuid = "7426afd6-0234-4701-9045-83dfd39166e6";
        let uuid2 = "abcdef01-2345-6789-abcd-ef0123456789";

        // UpdateEntity — patch is now a structured ProposalEntityPatch object
        let v =
            serde_json::json!({"kind": "update_entity", "id": uuid, "patch": {"name": "NewName"}});
        let cs: ProposalChangeset =
            serde_json::from_value(v).expect("UpdateEntity must deserialize from Value");
        assert!(
            matches!(cs, ProposalChangeset::UpdateEntity { .. }),
            "expected UpdateEntity"
        );

        // AddEdge
        let v = serde_json::json!({
            "kind": "add_edge",
            "source": uuid, "target": uuid2,
            "relation": "extends", "weight": 1.0
        });
        let cs: ProposalChangeset =
            serde_json::from_value(v).expect("AddEdge must deserialize from Value");
        assert!(
            matches!(cs, ProposalChangeset::AddEdge { .. }),
            "expected AddEdge"
        );

        // MergeEntities
        let v = serde_json::json!({"kind": "merge_entities", "into": uuid, "from": uuid2});
        let cs: ProposalChangeset =
            serde_json::from_value(v).expect("MergeEntities must deserialize from Value");
        assert!(
            matches!(cs, ProposalChangeset::MergeEntities { .. }),
            "expected MergeEntities"
        );

        // SupersedeEntity
        let v = serde_json::json!({"kind": "supersede_entity", "old": uuid, "new": uuid2});
        let cs: ProposalChangeset =
            serde_json::from_value(v).expect("SupersedeEntity must deserialize from Value");
        assert!(
            matches!(cs, ProposalChangeset::SupersedeEntity { .. }),
            "expected SupersedeEntity"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn proposal_changeset_rejects_invalid_edge_weight() {
        let uuid = "7426afd6-0234-4701-9045-83dfd39166e6";
        let uuid2 = "abcdef01-2345-6789-abcd-ef0123456789";

        let v = serde_json::json!({
            "kind": "add_edge",
            "source": uuid, "target": uuid2,
            "relation": "extends", "weight": 2.0
        });
        let result: Result<ProposalChangeset, _> = serde_json::from_value(v);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("[0.0, 1.0]"),
            "error should mention range: {err}"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn proposal_changeset_accepts_null_edge_weight() {
        let uuid = "7426afd6-0234-4701-9045-83dfd39166e6";
        let uuid2 = "abcdef01-2345-6789-abcd-ef0123456789";

        let v = serde_json::json!({
            "kind": "add_edge",
            "source": uuid, "target": uuid2,
            "relation": "extends", "weight": null
        });
        let cs: ProposalChangeset =
            serde_json::from_value(v).expect("null weight should be accepted");
        assert!(matches!(
            cs,
            ProposalChangeset::AddEdge { weight: None, .. }
        ));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn rerank_payload_serde_rejects_non_finite_score() {
        let json = serde_json::json!({
            "served_by_profile_id": null,
            "model_id": "00000000-0000-0000-0000-000000000001",
            "candidates": [],
            "reranked": [],
            "final_scores": [["00000000-0000-0000-0000-000000000001", "Infinity"]],
            "latency_us": 100,
            "hook_applied": false,
            "hook_target_match": false
        });
        let result: Result<RerankExecutedPayload, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn rerank_payload_is_valid_checks_finite() {
        let p = RerankExecutedPayload {
            served_by_profile_id: None,
            model_id: Id128::from_u128(1),
            candidates: Vec::new(),
            reranked: Vec::new(),
            final_scores: alloc::vec![(Id128::from_u128(1), 0.5)],
            latency_us: 100,
            hook_applied: false,
            hook_target_match: false,
        };
        assert!(p.is_valid());

        let p_inf = RerankExecutedPayload {
            served_by_profile_id: None,
            model_id: Id128::from_u128(1),
            candidates: Vec::new(),
            reranked: Vec::new(),
            final_scores: alloc::vec![(Id128::from_u128(1), f32::INFINITY)],
            latency_us: 100,
            hook_applied: false,
            hook_target_match: false,
        };
        assert!(!p_inf.is_valid());
    }
}
