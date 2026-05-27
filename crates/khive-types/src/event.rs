//! Event substrate — universal system log.
//!
//! Every verb execution produces an Event. Audit, usage metering, derived
//! state, and evolutionary learning (edge reinforcement, traversal history)
//! are all computed via Fold over the Event stream.

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EventOutcome {
    #[default]
    Success,
    Denied,
    Error,
}

impl EventOutcome {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EventKind {
    Audit,
    RecallExecuted,
    RerankExecuted,
    SearchExecuted,
    LinkCreated,
    EntityCreated,
    EntityUpdated,
    EntityDeleted,
    EntityMerged,
    NoteCreated,
    NoteUpdated,
    NoteDeleted,
    EdgeUpdated,
    EdgeDeleted,
    TaskTransitioned,
    FeedbackExplicit,
    ProfileResolutionRecommended,
    ProfileMerged,
    EmbeddingModelChanged,
    EmbeddingMigrationCompleted,
    EmbeddingMigrationFailed,
    EmbeddingDriftDetected,
    ProposalCreated,
    ProposalReviewed,
    ProposalApplied,
    ProposalWithdrawn,
}

impl EventKind {
    pub const ALL: [Self; 26] = [
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
    ];

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
            other => Err(crate::error::UnknownVariant::new(
                "event_kind",
                other,
                EVENT_KIND_VALID,
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AggregateRef {
    pub kind: String,
    pub id: Id128,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(tag = "kind", content = "payload", rename_all = "snake_case")
)]
pub enum EventPayload {
    Json(String),
    RerankExecuted(RerankExecutedPayload),
    #[cfg(feature = "serde")]
    ProposalCreated(ProposalCreatedPayload),
    ProposalReviewed(ProposalReviewedPayload),
    ProposalApplied(ProposalAppliedPayload),
    ProposalWithdrawn(ProposalWithdrawnPayload),
}

impl Default for EventPayload {
    fn default() -> Self {
        Self::Json("{}".into())
    }
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RerankExecutedPayload {
    pub served_by_profile_id: Option<String>,
    pub model_id: Id128,
    pub candidates: Vec<Id128>,
    pub reranked: Vec<(Id128, Vec<(String, f32)>)>,
    pub final_scores: Vec<(Id128, f32)>,
    pub latency_us: u64,
    pub hook_applied: bool,
    pub hook_target_match: bool,
}

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

/// Structured draft for adding a new entity via a proposal (ADR-046:100).
///
/// Fields mirror the `create(kind=<entity kind>)` verb surface; `kind` is
/// validated against the closed 8-kind taxonomy (ADR-001) at apply time.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EntityDraft {
    /// Entity kind — must be one of the 8 closed ADR-001 values.
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

/// Structured patch for modifying an existing entity via a proposal (ADR-046:101).
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

/// Structured draft for adding a new note via a proposal (ADR-046:106).
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

#[cfg(feature = "serde")]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposalChangeset {
    /// Add a new entity. `entity.kind` validated against ADR-001 at apply time.
    AddEntity {
        entity: EntityDraft,
    },
    /// Modify an existing entity's properties / tags / description.
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

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProposalReviewedPayload {
    pub proposal_id: Id128,
    pub reviewer: String,
    pub decision: ProposalDecision,
    pub comment: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProposalDecision {
    Approve,
    Reject,
    Comment,
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

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ProposalAppliedPayload {
    pub proposal_id: Id128,
    pub applied_at: crate::Timestamp,
    pub applied_by: String,
    pub result: ApplyResult,
}

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

    pub fn kind(mut self, kind: EventKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn payload(mut self, payload: EventPayload) -> Self {
        self.payload = payload;
        self
    }

    pub fn payload_schema_version(mut self, version: u32) -> Self {
        self.payload_schema_version = version;
        self
    }

    pub fn profile_state_version(mut self, version: u64) -> Self {
        self.profile_state_version = Some(version);
        self
    }

    pub fn aggregate(mut self, aggregate: AggregateRef) -> Self {
        self.aggregate = Some(aggregate);
        self
    }

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
            reviewer: "ocean".into(),
            decision: ProposalDecision::Approve,
            comment: None,
        });
        let event = EventBuilder::new("review", SubstrateKind::Entity, "ocean")
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
}
