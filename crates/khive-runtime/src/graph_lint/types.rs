//! Typed graph-lint response and settlement evidence (ADR-048).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LintRuleId {
    ConceptNoParent,
    DanglingEdge,
    DirectionViolation,
    DuplicateName,
    LowWeightEdge,
    MinEdgeDensity,
    MissingEntityType,
    MissingImplements,
    MissingIntroducedBy,
    NoteNoAnnotates,
    OrphanEntity,
    PaperMissingMetadata,
    SupersededNotMarked,
}
impl LintRuleId {
    pub const ALL: [Self; 13] = [
        Self::ConceptNoParent,
        Self::DanglingEdge,
        Self::DirectionViolation,
        Self::DuplicateName,
        Self::LowWeightEdge,
        Self::MinEdgeDensity,
        Self::MissingEntityType,
        Self::MissingImplements,
        Self::MissingIntroducedBy,
        Self::NoteNoAnnotates,
        Self::OrphanEntity,
        Self::PaperMissingMetadata,
        Self::SupersededNotMarked,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConceptNoParent => "concept-no-parent",
            Self::DanglingEdge => "dangling-edge",
            Self::DirectionViolation => "direction-violation",
            Self::DuplicateName => "duplicate-name",
            Self::LowWeightEdge => "low-weight-edge",
            Self::MinEdgeDensity => "min-edge-density",
            Self::MissingEntityType => "missing-entity-type",
            Self::MissingImplements => "missing-implements",
            Self::MissingIntroducedBy => "missing-introduced-by",
            Self::NoteNoAnnotates => "note-no-annotates",
            Self::OrphanEntity => "orphan-entity",
            Self::PaperMissingMetadata => "paper-missing-metadata",
            Self::SupersededNotMarked => "superseded-not-marked",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LintSeverity {
    Info,
    Warn,
    Error,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "substrate", rename_all = "lowercase")]
pub enum LintSubject {
    Entity {
        namespace: String,
        entity_id: Uuid,
        entity_name: String,
        entity_kind: String,
    },
    Edge {
        namespace: String,
        edge_id: Uuid,
    },
    Note {
        namespace: String,
        note_id: Uuid,
        note_kind: String,
    },
}
impl LintSubject {
    pub fn namespace(&self) -> &str {
        match self {
            Self::Entity { namespace, .. }
            | Self::Edge { namespace, .. }
            | Self::Note { namespace, .. } => namespace,
        }
    }
    pub fn id(&self) -> Uuid {
        match self {
            Self::Entity { entity_id, .. } => *entity_id,
            Self::Edge { edge_id, .. } => *edge_id,
            Self::Note { note_id, .. } => *note_id,
        }
    }
    pub fn substrate(&self) -> &'static str {
        match self {
            Self::Entity { .. } => "entity",
            Self::Edge { .. } => "edge",
            Self::Note { .. } => "note",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LintViolation {
    #[serde(flatten)]
    pub subject: LintSubject,
    pub rule: LintRuleId,
    pub severity: LintSeverity,
    pub message: String,
    pub suggestion: String,
    pub auto_fixable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_ids: Option<Vec<Uuid>>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintSummary {
    pub total: u64,
    pub errors: u64,
    pub warnings: u64,
    pub infos: u64,
    pub by_rule: BTreeMap<LintRuleId, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fixed: Option<u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyCoverageEntry {
    pub present: u64,
    pub total: u64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyCoverage {
    pub entities: BTreeMap<String, PropertyCoverageEntry>,
    pub notes: BTreeMap<String, PropertyCoverageEntry>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LintStats {
    pub total_entities: u64,
    pub total_notes: u64,
    pub total_edges: u64,
    pub entity_kinds: BTreeMap<String, u64>,
    pub note_kinds: BTreeMap<String, u64>,
    pub edge_relations: BTreeMap<String, u64>,
    pub avg_density: f64,
    pub density_histogram: BTreeMap<String, u64>,
    pub property_coverage: PropertyCoverage,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LintReport {
    pub violations: Vec<LintViolation>,
    pub summary: LintSummary,
    pub stats: LintStats,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixAction {
    SoftDeleteEdge,
    ProposeDuplicateMerge,
    MarkDeprecated,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionIdentity {
    pub violation_index: u64,
    pub action: FixAction,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FixReceipt {
    Committed {
        #[serde(flatten)]
        action: CommittedFixAction,
    },
    Conflict {
        #[serde(flatten)]
        identity: ActionIdentity,
        reason: String,
    },
    Unsupported {
        #[serde(flatten)]
        identity: ActionIdentity,
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CommittedFixAction {
    SoftDeleteEdge {
        violation_index: u64,
    },
    MarkDeprecated {
        violation_index: u64,
    },
    ProposeDuplicateMerge {
        violation_index: u64,
        question_note_id: Uuid,
    },
}
impl FixReceipt {
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LintFixOutcome {
    Unknown,
    Failed,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LintFixFailureDetail {
    pub outcome: LintFixOutcome,
    pub failed_action: ActionIdentity,
    pub confirmed_receipts: Vec<FixReceipt>,
    pub report: LintReport,
}
