use super::{
    bounded, bounded_to, invalid, MAX_AUTHORITY_MANIFEST_BYTES, MAX_AUTHORITY_RECORD_BYTES,
    MAX_AUTHORITY_VALUE_BYTES,
};
use khive_storage::types::{SqlRow, SqlValue, StorageResult};
use uuid::Uuid;

/// Immutable initialization data; the manifest contains no key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityEpoch {
    pub epoch_id: Uuid,
    pub marker_id: Uuid,
    pub store_id: Uuid,
    pub manifest_digest: [u8; 32],
    pub manifest: Vec<u8>,
    pub namespace: String,
    pub created_at: i64,
}

/// A persistence role, not a permission type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Grant,
    Anchor,
}

impl RecordKind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Anchor => "anchor",
        }
    }
}

/// Immutable canonical data and its indexed lookup columns.
/// The authority service must authenticate the body and compare every index
/// column with it before treating a returned record as authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityRecord {
    pub id: Uuid,
    pub epoch_id: Uuid,
    pub kind: RecordKind,
    pub principal: Vec<u8>,
    pub namespace: String,
    /// Sorted unique opaque type tags for every right in the authenticated body.
    pub right_types: Vec<String>,
    pub parent_id: Option<Uuid>,
    pub bootstrap_slot: Option<String>,
    pub root_anchor_id: Option<Uuid>,
    pub created_at: i64,
    pub body: Vec<u8>,
}

impl AuthorityRecord {
    pub(super) fn validate(&self) -> StorageResult<()> {
        bounded("principal", &self.principal)?;
        bounded("namespace", self.namespace.as_bytes())?;
        bounded_to("record body", &self.body, MAX_AUTHORITY_RECORD_BYTES)?;
        match self.kind {
            RecordKind::Anchor
                if self.right_types.is_empty()
                    && self.parent_id.is_none()
                    && self.bootstrap_slot.is_none()
                    && self.root_anchor_id.is_none() =>
            {
                Ok(())
            }
            RecordKind::Grant
                if self.root_anchor_id.is_some()
                    && self.root_anchor_id != Some(self.id)
                    && self.parent_id != Some(self.id)
                    && (self.parent_id.is_some() != self.bootstrap_slot.is_some()) =>
            {
                if self.right_types.is_empty() || self.right_types.len() > 256 {
                    return Err(invalid("grant right-type count must be 1..=256"));
                }
                if self
                    .right_types
                    .iter()
                    .any(|tag| tag.is_empty() || tag.len() > 128)
                    || self.right_types.windows(2).any(|pair| pair[0] >= pair[1])
                {
                    return Err(invalid(
                        "right-type tags must be sorted, unique and 1..=128 bytes",
                    ));
                }
                if let Some(slot) = &self.bootstrap_slot {
                    bounded("bootstrap slot", slot.as_bytes())?;
                }
                Ok(())
            }
            _ => Err(invalid("record role and lineage columns disagree")),
        }
    }
}

/// The directly affected record represented by one core event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Initialization,
    Grant,
    Anchor,
}
impl ChangeKind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Initialization => "initialization",
            Self::Grant => "grant",
            Self::Anchor => "anchor",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityChange {
    pub event_id: Uuid,
    pub epoch_id: Uuid,
    pub action_id: Uuid,
    pub kind: ChangeKind,
    pub record_id: Uuid,
}

/// Terminal storage state; which state applies is decided by the authority service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalState {
    Disabled,
    Revoked,
    Retired,
}
impl TerminalState {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Revoked => "revoked",
            Self::Retired => "retired",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityTransition {
    pub id: Uuid,
    pub epoch_id: Uuid,
    pub record_id: Uuid,
    pub state: TerminalState,
    pub action_id: Uuid,
    pub event_id: Uuid,
    pub created_at: i64,
    pub body: Vec<u8>,
}

/// Exact replay identity; the caller is canonical structural data, not a display label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayKey {
    pub epoch_id: Uuid,
    pub structural_caller: Vec<u8>,
    pub canonical_verb: String,
    pub request_id: String,
}
impl ReplayKey {
    pub(super) fn params(&self) -> StorageResult<Vec<SqlValue>> {
        bounded("structural caller", &self.structural_caller)?;
        bounded("canonical verb", self.canonical_verb.as_bytes())?;
        bounded("request id", self.request_id.as_bytes())?;
        Ok(vec![
            id(self.epoch_id),
            SqlValue::Blob(self.structural_caller.clone()),
            SqlValue::Text(self.canonical_verb.clone()),
            SqlValue::Text(self.request_id.clone()),
        ])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityReplay {
    pub canonical_request: Vec<u8>,
    pub result: Vec<u8>,
}

/// Stable ascending UUID page. A continuation is present only when another row exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityPage<T> {
    pub items: Vec<T>,
    pub next_id: Option<Uuid>,
}

pub(super) fn id(value: Uuid) -> SqlValue {
    SqlValue::Text(value.to_string())
}
pub(super) fn optional_id(value: Option<Uuid>) -> SqlValue {
    value.map_or(SqlValue::Null, id)
}
pub(super) fn text(row: &SqlRow, name: &str) -> StorageResult<String> {
    row.text(name)
        .map(str::to_owned)
        .map_err(|e| invalid(e.to_string()))
}
pub(super) fn optional_text(row: &SqlRow, name: &str) -> StorageResult<Option<String>> {
    row.opt_text(name)
        .map(|v| v.map(str::to_owned))
        .map_err(|e| invalid(e.to_string()))
}
pub(super) fn integer(row: &SqlRow, name: &str) -> StorageResult<i64> {
    row.i64(name).map_err(|e| invalid(e.to_string()))
}
pub(super) fn blob(row: &SqlRow, name: &str) -> StorageResult<Vec<u8>> {
    blob_with_limit(row, name, MAX_AUTHORITY_VALUE_BYTES)
}
pub(super) fn blob_with_limit(row: &SqlRow, name: &str, maximum: usize) -> StorageResult<Vec<u8>> {
    match row.get(name) {
        Some(SqlValue::Blob(v)) => {
            bounded_to(name, v, maximum)?;
            Ok(v.clone())
        }
        _ => Err(invalid(format!("expected blob column {name}"))),
    }
}
fn parse_id(value: &str) -> StorageResult<Uuid> {
    let id = Uuid::parse_str(value).map_err(|_| invalid("invalid stored UUID"))?;
    if id.to_string() != value {
        return Err(invalid("noncanonical stored UUID"));
    }
    Ok(id)
}
fn row_id(row: &SqlRow, name: &str) -> StorageResult<Uuid> {
    parse_id(&text(row, name)?)
}
fn optional_row_id(row: &SqlRow, name: &str) -> StorageResult<Option<Uuid>> {
    optional_text(row, name)?
        .as_deref()
        .map(parse_id)
        .transpose()
}

pub(super) fn epoch(row: SqlRow) -> StorageResult<AuthorityEpoch> {
    let digest = blob(&row, "manifest_digest")?;
    Ok(AuthorityEpoch {
        epoch_id: row_id(&row, "epoch_id")?,
        marker_id: row_id(&row, "marker_id")?,
        store_id: row_id(&row, "store_id")?,
        manifest_digest: digest
            .try_into()
            .map_err(|_| invalid("manifest digest must be 32 bytes"))?,
        manifest: blob_with_limit(&row, "manifest", MAX_AUTHORITY_MANIFEST_BYTES)?,
        namespace: text(&row, "namespace")?,
        created_at: integer(&row, "created_at")?,
    })
}
pub(super) fn record(row: SqlRow) -> StorageResult<AuthorityRecord> {
    let kind = match row
        .text("record_kind")
        .map_err(|e| invalid(e.to_string()))?
    {
        "grant" => RecordKind::Grant,
        "anchor" => RecordKind::Anchor,
        _ => return Err(invalid("unknown stored record role")),
    };
    let record = AuthorityRecord {
        id: row_id(&row, "record_id")?,
        epoch_id: row_id(&row, "epoch_id")?,
        kind,
        principal: blob(&row, "principal")?,
        namespace: text(&row, "namespace")?,
        right_types: serde_json::from_str(&text(&row, "right_types")?)
            .map_err(|_| invalid("invalid right-type index result"))?,
        parent_id: optional_row_id(&row, "parent_id")?,
        bootstrap_slot: optional_text(&row, "bootstrap_slot")?,
        root_anchor_id: optional_row_id(&row, "root_anchor_id")?,
        created_at: integer(&row, "created_at")?,
        body: blob_with_limit(&row, "body", MAX_AUTHORITY_RECORD_BYTES)?,
    };
    record.validate()?;
    Ok(record)
}
pub(super) fn change(row: SqlRow) -> StorageResult<AuthorityChange> {
    let kind = match row
        .text("record_kind")
        .map_err(|e| invalid(e.to_string()))?
    {
        "initialization" => ChangeKind::Initialization,
        "grant" => ChangeKind::Grant,
        "anchor" => ChangeKind::Anchor,
        _ => return Err(invalid("unknown stored change role")),
    };
    Ok(AuthorityChange {
        event_id: row_id(&row, "event_id")?,
        epoch_id: row_id(&row, "epoch_id")?,
        action_id: row_id(&row, "action_id")?,
        kind,
        record_id: row_id(&row, "record_id")?,
    })
}
pub(super) fn transition(row: SqlRow) -> StorageResult<AuthorityTransition> {
    let state = match row
        .text("terminal_state")
        .map_err(|e| invalid(e.to_string()))?
    {
        "disabled" => TerminalState::Disabled,
        "revoked" => TerminalState::Revoked,
        "retired" => TerminalState::Retired,
        _ => return Err(invalid("unknown stored terminal state")),
    };
    Ok(AuthorityTransition {
        id: row_id(&row, "transition_id")?,
        epoch_id: row_id(&row, "epoch_id")?,
        record_id: row_id(&row, "record_id")?,
        state,
        action_id: row_id(&row, "action_id")?,
        event_id: row_id(&row, "event_id")?,
        created_at: integer(&row, "created_at")?,
        body: blob(&row, "body")?,
    })
}

pub(super) fn change_event(
    row: SqlRow,
) -> StorageResult<(AuthorityChange, khive_storage::event::Event)> {
    use khive_types::{EventKind, EventOutcome, RefResolution, SubstrateKind};
    if text(&row, "kind")? != EventKind::GrantChange.to_string()
        || text(&row, "substrate")? != SubstrateKind::Event.to_string()
        || text(&row, "outcome")? != EventOutcome::Success.to_string()
        || integer(&row, "payload_schema_version")? != 1
    {
        return Err(invalid(
            "authority history requires successful schema1 GrantChange Event",
        ));
    }
    fn optional_integer(row: &SqlRow, name: &str) -> StorageResult<Option<i64>> {
        match row.get(name) {
            Some(SqlValue::Null) => Ok(None),
            Some(SqlValue::Integer(value)) => Ok(Some(*value)),
            _ => Err(invalid(format!("expected nullable integer column {name}"))),
        }
    }
    let profile_state_version = optional_integer(&row, "profile_state_version")?
        .map(|value| u64::try_from(value).map_err(|_| invalid("negative profile state version")))
        .transpose()?;
    let op_index = optional_integer(&row, "op_index")?
        .map(|value| u32::try_from(value).map_err(|_| invalid("operation index outside u32 range")))
        .transpose()?;
    let ref_resolution = optional_text(&row, "ref_resolution")?
        .map(|value| {
            value
                .parse::<RefResolution>()
                .map_err(|_| invalid("invalid reference resolution"))
        })
        .transpose()?;
    if op_index.is_some() != ref_resolution.is_some() {
        return Err(invalid(
            "event operation attribution must be present or absent together",
        ));
    }
    let event = khive_storage::event::Event {
        id: row_id(&row, "id")?,
        namespace: text(&row, "namespace")?,
        verb: text(&row, "verb")?,
        substrate: SubstrateKind::Event,
        actor: text(&row, "actor")?,
        kind: EventKind::GrantChange,
        outcome: EventOutcome::Success,
        payload: serde_json::from_str(&text(&row, "payload")?)
            .map_err(|_| invalid("invalid authority event JSON"))?,
        payload_schema_version: 1,
        profile_state_version,
        duration_us: integer(&row, "duration_us")?,
        target_id: optional_row_id(&row, "target_id")?,
        session_id: optional_row_id(&row, "session_id")?,
        aggregate_kind: optional_text(&row, "aggregate_kind")?,
        aggregate_id: optional_row_id(&row, "aggregate_id")?,
        created_at: integer(&row, "created_at")?,
        op_index,
        ref_resolution,
    };
    let change = change(row)?;
    if event.id != change.event_id {
        return Err(invalid(
            "event and authority association identities disagree",
        ));
    }
    Ok((change, event))
}
