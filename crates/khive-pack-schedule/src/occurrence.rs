//! Durable identities and immutable definitions for version-three schedules.

use chrono::{DateTime, Utc};
use khive_runtime::{NamespaceToken, RuntimeError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::misfire::{parse_v3_misfire_policy, MisfirePolicy, V3MisfirePolicy};

/// The maximum number of new occurrence identities in one host drain pass.
pub const PASS_MATERIALIZATION_LIMIT: usize = 200;
/// Marker used only to route display notes away from the legacy executor.
pub const RECEIPT_VERSION_PROPERTY: &str = "receipt_version";

/// A UTC instant retaining the precision of the original schedule parser.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceInstant {
    pub seconds: i64,
    pub nanos: u32,
}

impl OccurrenceInstant {
    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        Self {
            seconds: value.timestamp(),
            nanos: value.timestamp_subsec_nanos(),
        }
    }

    pub fn datetime(self) -> Result<DateTime<Utc>, RuntimeError> {
        DateTime::from_timestamp(self.seconds, self.nanos)
            .ok_or_else(|| RuntimeError::InvalidInput("unrepresentable schedule instant".into()))
    }

    pub fn from_micros(value: i64) -> Result<Self, RuntimeError> {
        DateTime::from_timestamp_micros(value)
            .map(Self::from_datetime)
            .ok_or_else(|| RuntimeError::InvalidInput("unrepresentable schedule clock".into()))
    }

    pub(crate) fn nanos_since_epoch(self) -> i128 {
        i128::from(self.seconds) * 1_000_000_000 + i128::from(self.nanos)
    }

    pub fn within_bound(self, decision_us: i64, bound_seconds: i64) -> bool {
        let elapsed = i128::from(decision_us) * 1_000 - self.nanos_since_epoch();
        elapsed >= 0 && elapsed <= i128::from(bound_seconds) * 1_000_000_000
    }
}

/// The original dispatch-minted creator, recorded structurally rather than parsed from a label.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionActor {
    pub kind: String,
    pub id: String,
}

impl DefinitionActor {
    pub fn label(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduledPayload {
    Reminder { content: String },
    Action { action: String },
}

/// Immutable, complete declaration. Only the pack's validated declaration path can activate it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionV3 {
    pub schema_version: u8,
    pub schedule_id: Uuid,
    pub definition_revision: i64,
    pub creator: DefinitionActor,
    pub namespace: String,
    pub payload: ScheduledPayload,
    pub first_trigger_text: String,
    pub first_trigger: OccurrenceInstant,
    pub repeat: Option<String>,
    pub monthly_anchor: OccurrenceInstant,
    pub misfire_policy: String,
    pub requested_max_lateness_s: Option<i64>,
    pub recipient: DefinitionActor,
}

impl DefinitionV3 {
    pub(crate) fn from_declaration(
        token: &NamespaceToken,
        schedule_id: Uuid,
        payload: ScheduledPayload,
        first_trigger_text: String,
        first_trigger: DateTime<Utc>,
        repeat: Option<String>,
        policy: V3MisfirePolicy,
    ) -> Self {
        let creator = DefinitionActor {
            kind: token.actor().kind.to_string(),
            id: token.actor().id.to_string(),
        };
        let misfire_policy = match policy.kind() {
            MisfirePolicy::Skip => "skip",
            MisfirePolicy::CatchUpOnce { .. } => "catch_up_once",
        }
        .to_owned();
        let requested_max_lateness_s = policy.requested_max_lateness_s();
        Self {
            schema_version: 3,
            schedule_id,
            definition_revision: 1,
            recipient: creator.clone(),
            creator,
            namespace: token.namespace().as_str().to_owned(),
            payload,
            first_trigger_text,
            first_trigger: OccurrenceInstant::from_datetime(first_trigger),
            repeat,
            monthly_anchor: OccurrenceInstant::from_datetime(first_trigger),
            misfire_policy,
            requested_max_lateness_s,
        }
    }

    pub fn policy(&self) -> Result<MisfirePolicy, RuntimeError> {
        let mut value = serde_json::json!({"misfire_policy": self.misfire_policy});
        if let Some(bound) = self.requested_max_lateness_s {
            value["max_lateness"] = bound.into();
        }
        parse_v3_misfire_policy(&value)
            .map(|policy| policy.kind())
            .map_err(RuntimeError::InvalidInput)
    }

    /// Fixed struct field order and closed, map-free fields define the canonical representation.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeError> {
        serde_json::to_vec(self)
            .map_err(|e| RuntimeError::Internal(format!("encode schedule definition: {e}")))
    }

    pub fn digest(&self) -> Result<[u8; 32], RuntimeError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"khive.schedule.definition.v3\0");
        hasher.update(&self.canonical_bytes()?);
        Ok(*hasher.finalize().as_bytes())
    }

    pub(crate) fn authenticate(bytes: &[u8], digest: &[u8]) -> Result<Self, RuntimeError> {
        let definition: Self = serde_json::from_slice(bytes)
            .map_err(|e| RuntimeError::Internal(format!("decode schedule definition: {e}")))?;
        if definition.schema_version != 3
            || definition.definition_revision != 1
            || definition.canonical_bytes()? != bytes
            || definition.digest()?.as_slice() != digest
        {
            return Err(RuntimeError::Internal(
                "schedule definition binding mismatch".into(),
            ));
        }
        definition.first_trigger.datetime()?;
        definition.monthly_anchor.datetime()?;
        definition.policy()?;
        if let Some(repeat) = &definition.repeat {
            crate::repeat::parse_repeat(repeat).map_err(RuntimeError::InvalidInput)?;
        }
        Ok(definition)
    }
}

/// A complete occurrence identity. Store identity and definition revision prevent accidental reuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OccurrenceKey {
    pub store_id: Uuid,
    pub schedule_id: Uuid,
    pub definition_revision: i64,
    pub scheduled_for: OccurrenceInstant,
}

impl OccurrenceKey {
    pub fn id(self) -> Uuid {
        let mut bytes = Vec::with_capacity(96);
        bytes.extend_from_slice(b"khive.schedule.occurrence.v3\0");
        bytes.extend_from_slice(self.store_id.as_bytes());
        bytes.extend_from_slice(self.schedule_id.as_bytes());
        bytes.extend_from_slice(&self.definition_revision.to_be_bytes());
        bytes.extend_from_slice(&self.scheduled_for.seconds.to_be_bytes());
        bytes.extend_from_slice(&self.scheduled_for.nanos.to_be_bytes());
        Uuid::new_v5(&Uuid::NAMESPACE_OID, &bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalOutcome {
    Succeeded,
    Failed,
    TimedOut,
    Held,
    Missed,
    Refused,
    SpawnFailed,
    Cancelled,
    Indeterminate,
}

impl TerminalOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Held => "held",
            Self::Missed => "missed",
            Self::Refused => "refused",
            Self::SpawnFailed => "spawn_failed",
            Self::Cancelled => "cancelled",
            Self::Indeterminate => "indeterminate",
        }
    }
}

/// Remaining new-row allowance, shared by every series in one host pass.
#[derive(Debug)]
pub struct PassBudget {
    remaining: usize,
}
impl Default for PassBudget {
    fn default() -> Self {
        Self {
            remaining: PASS_MATERIALIZATION_LIMIT,
        }
    }
}
impl PassBudget {
    pub fn remaining(&self) -> usize {
        self.remaining
    }
    pub fn consume(&mut self, created: usize) -> Result<(), RuntimeError> {
        self.remaining = self.remaining.checked_sub(created).ok_or_else(|| {
            RuntimeError::Internal("schedule occurrence pass budget exceeded".into())
        })?;
        Ok(())
    }
}

/// Original namespace and immutable definition, with the current accounting position.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SeriesSnapshot {
    pub store_id: Uuid,
    pub schedule_id: Uuid,
    pub definition_revision: i64,
    pub namespace: String,
    pub revision: i64,
    pub state: String,
    pub next: Option<OccurrenceInstant>,
    pub window_source: Option<OccurrenceInstant>,
    pub horizon: Option<OccurrenceInstant>,
    pub selected: Option<OccurrenceInstant>,
    pub selection_at_us: Option<i64>,
    pub effective_max_lateness_s: Option<i64>,
    pub active_occurrence_id: Option<Uuid>,
    pub active_invocation_id: Option<Uuid>,
    pub suspended_invocation_id: Option<Uuid>,
    pub cancellation_cut_us: Option<i64>,
    pub accounting_blocked_reason: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub enum WorkPurpose {
    AccountingOrRecovery,
    UnresolvedEvidence,
}
impl WorkPurpose {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AccountingOrRecovery => "accounting_or_recovery",
            Self::UnresolvedEvidence => "unresolved_evidence",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuxiliaryWork {
    pub series: SeriesSnapshot,
    pub ready_unclaimed: Option<Uuid>,
    pub expired_attempt: bool,
}

#[derive(Clone, Debug)]
pub struct WorkPage {
    pub items: Vec<AuxiliaryWork>,
    pub next: Option<Uuid>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OccurrenceObservation {
    pub id: Uuid,
    pub key: OccurrenceKey,
    pub namespace: String,
    pub materialized_at_us: i64,
    pub selection_at_us: i64,
    pub effective_max_lateness_s: i64,
    pub terminal: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OccurrencePage {
    pub items: Vec<OccurrenceObservation>,
    pub has_more: bool,
}

/// Scheduler ownership only; not an authorization token for the stored action.
#[derive(Clone, Debug)]
pub struct ClaimHandle {
    pub(crate) store_id: Uuid,
    pub(crate) occurrence: OccurrenceObservation,
    pub(crate) invocation_id: Uuid,
    pub(crate) generation: i64,
    pub(crate) owner_nonce: Uuid,
}
impl ClaimHandle {
    pub fn occurrence(&self) -> &OccurrenceObservation {
        &self.occurrence
    }
    pub fn invocation_id(&self) -> Uuid {
        self.invocation_id
    }
}

#[derive(Clone, Debug)]
pub struct LaunchIntent {
    pub(crate) claim: ClaimHandle,
    pub(crate) intent_id: Uuid,
    pub(crate) admission_at_us: i64,
}
impl LaunchIntent {
    pub fn claim(&self) -> &ClaimHandle {
        &self.claim
    }
    pub fn id(&self) -> Uuid {
        self.intent_id
    }
    pub fn admission_at_us(&self) -> i64 {
        self.admission_at_us
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedResult {
    pub outcome: TerminalOutcome,
    pub reason_code: String,
    pub evidence: serde_json::Value,
    pub exec_receipt_id: Option<Uuid>,
    pub started_at_us: Option<i64>,
    pub completed_at_us: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit_signal: Option<i64>,
}
impl ObservedResult {
    pub fn non_invoked(outcome: TerminalOutcome, reason: &str) -> Self {
        Self {
            outcome,
            reason_code: reason.to_owned(),
            evidence: serde_json::json!({"reason":reason}),
            exec_receipt_id: None,
            started_at_us: None,
            completed_at_us: None,
            exit_code: None,
            exit_signal: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TerminalReceipt {
    pub id: Uuid,
    pub occurrence_id: Uuid,
    pub outcome: String,
    pub reason_code: String,
    pub invocation_id: Option<Uuid>,
    pub recorded_at_us: i64,
    pub final_decision_at_us: Option<i64>,
    pub evidence: serde_json::Value,
}

/// Advance one nominal occurrence without restarting a calendar walk at the original anchor.
pub(crate) fn next_nominal(
    repeat: &crate::repeat::Repeat,
    anchor: DateTime<Utc>,
    current: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    use chrono::{Datelike, Months};
    if !matches!(repeat, crate::repeat::Repeat::Monthly) {
        return repeat.next_after(current);
    }
    let months = i64::from(current.year() - anchor.year()) * 12 + i64::from(current.month())
        - i64::from(anchor.month());
    let mut months = u32::try_from(months).ok()?;
    let mut candidate = anchor.checked_add_months(Months::new(months))?;
    if candidate <= current {
        months = months.checked_add(1)?;
        candidate = anchor.checked_add_months(Months::new(months))?;
    }
    (candidate > current).then_some(candidate)
}

#[derive(Clone, Debug)]
pub enum Admission {
    Launch(LaunchIntent),
    Settled(TerminalReceipt),
    Lost,
}
