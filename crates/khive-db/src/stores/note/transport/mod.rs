//! Durable sender-side records. No transport adapter performs SQL.
use super::{map_err, SqlNoteStore};
use crate::pool::ConnectionPool;
use khive_storage::{StorageCapability, StorageError, StorageResult};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvelopeKey {
    pub logical_message_id: Uuid,
    pub recipient_device_id: Uuid,
    pub recipient_key_epoch: u64,
}

/// Immutable submission data. Credential references name a key-facility entry; never supply key
/// material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderEnvelope {
    pub namespace: String,
    pub logical_message_id: Uuid,
    pub outbound_note_id: Uuid,
    pub kind: String,
    pub slug: String,
    pub credential_ref: String,
    pub recipient_address: String,
    pub protocol_version: u32,
    pub sender_agent_id: String,
    pub recipient_agent_id: String,
    pub recipient_device_id: Uuid,
    pub recipient_key_epoch: u64,
    pub contact_generation: u64,
    pub sender_key_epoch: u64,
    pub recipient_key_fingerprint: String,
    pub enc: Vec<u8>,
    pub ciphertext: Vec<u8>,
}
impl SenderEnvelope {
    pub fn key(&self) -> EnvelopeKey {
        EnvelopeKey {
            logical_message_id: self.logical_message_id,
            recipient_device_id: self.recipient_device_id,
            recipient_key_epoch: self.recipient_key_epoch,
        }
    }
    pub fn validate(&self) -> StorageResult<()> {
        for id in [&self.sender_agent_id, &self.recipient_agent_id] {
            if Uuid::parse_str(id).ok().map(|id| id.to_string()).as_deref() != Some(id.as_str()) {
                return Err(invalid("agent id must be a canonical UUID"));
            }
        }
        if self.enc.len() != 32 || self.ciphertext.len() > 65_536 {
            return Err(invalid("invalid envelope byte lengths"));
        }
        if [
            self.recipient_key_epoch,
            self.sender_key_epoch,
            self.contact_generation,
        ]
        .iter()
        .any(|n| *n == 0 || *n > u32::MAX as u64)
        {
            return Err(invalid("epoch/generation outside supported range"));
        }
        if self.protocol_version != 1
            || self.kind.is_empty()
            || self.slug.is_empty()
            || self.credential_ref.is_empty()
        {
            return Err(invalid("invalid transport identity"));
        }
        if self.recipient_key_fingerprint.len() != 64
            || !self
                .recipient_key_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("fingerprint must be 32 lowercase hex bytes"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Pending,
    RecipientStored,
    RecipientQuarantined,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Transient,
    Authentication,
    Permanent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    InsufficientCredit,
    RecipientKeyChanged,
    /// Policy state evaluated at the refused transport attempt.
    PolicyDenied {
        mode: PolicyMode,
        revision: u64,
    },
}

/// Recorded evaluation mode; this does not enable or change runtime policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Off,
    Shadow,
    Enforce,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderRecord {
    pub envelope: SenderEnvelope,
    pub state: TransportState,
    pub attempt_count: u64,
    pub envelope_seq: u64,
    pub next_retry_at: Option<i64>,
    pub last_failure_class: Option<FailureClass>,
    pub hold_reason: Option<HoldReason>,
    pub receipt: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: "sender_transport".into(),
        message: message.into(),
    }
}
trait StorageSpelling {
    fn storage_spelling(&self) -> &'static str;
}
impl StorageSpelling for TransportState {
    fn storage_spelling(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::RecipientStored => "recipient_stored",
            Self::RecipientQuarantined => "recipient_quarantined",
            Self::Failed => "failed",
        }
    }
}
impl StorageSpelling for FailureClass {
    fn storage_spelling(&self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Authentication => "authentication",
            Self::Permanent => "permanent",
        }
    }
}
impl StorageSpelling for HoldReason {
    fn storage_spelling(&self) -> &'static str {
        match self {
            Self::InsufficientCredit => "insufficient_credit",
            Self::RecipientKeyChanged => "recipient_key_changed",
            Self::PolicyDenied { .. } => "policy_denied",
        }
    }
}
impl StorageSpelling for PolicyMode {
    fn storage_spelling(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}
fn encode(value: &impl StorageSpelling) -> &'static str {
    value.storage_spelling()
}
fn decode<T: serde::de::DeserializeOwned>(value: String) -> rusqlite::Result<T> {
    serde_json::from_value(Value::String(value)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}
fn uuid(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&row.get::<_, String>(index)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(e))
    })
}
const COLUMNS: &str = concat!(
    "namespace, logical_message_id, outbound_note_id, kind, slug, credential_ref, ",
    "recipient_address, protocol_version, sender_agent_id, recipient_agent_id, ",
    "recipient_device_id, recipient_key_epoch, contact_generation, sender_key_epoch, ",
    "recipient_key_fingerprint, enc, ciphertext, state, attempt_count, next_retry_at, ",
    "last_failure_class, hold_reason, receipt, created_at, updated_at, envelope_seq, ",
    "policy_mode, policy_revision",
);
fn unsigned_column(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}
fn read_unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    unsigned_column(row.get(index)?, index)
}
fn read_optional_unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| unsigned_column(value, index))
        .transpose()
}
fn sql_integer(value: u64) -> StorageResult<i64> {
    i64::try_from(value)
        .map_err(|_| invalid("unsigned transport value exceeds SQLite integer range"))
}
fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SenderRecord> {
    Ok(SenderRecord {
        envelope: SenderEnvelope {
            namespace: row.get(0)?,
            logical_message_id: uuid(row, 1)?,
            outbound_note_id: uuid(row, 2)?,
            kind: row.get(3)?,
            slug: row.get(4)?,
            credential_ref: row.get(5)?,
            recipient_address: row.get(6)?,
            protocol_version: row.get(7)?,
            sender_agent_id: row.get(8)?,
            recipient_agent_id: row.get(9)?,
            recipient_device_id: uuid(row, 10)?,
            recipient_key_epoch: read_unsigned(row, 11)?,
            contact_generation: read_unsigned(row, 12)?,
            sender_key_epoch: read_unsigned(row, 13)?,
            recipient_key_fingerprint: row.get(14)?,
            enc: row.get(15)?,
            ciphertext: row.get(16)?,
        },
        state: decode(row.get(17)?)?,
        attempt_count: read_unsigned(row, 18)?,
        next_retry_at: row.get(19)?,
        last_failure_class: row.get::<_, Option<String>>(20)?.map(decode).transpose()?,
        hold_reason: match row.get::<_, Option<String>>(21)?.as_deref() {
            Some("policy_denied") => Some(HoldReason::PolicyDenied {
                mode: decode(row.get(26)?)?,
                revision: read_unsigned(row, 27)?,
            }),
            reason => reason.map(|r| decode(r.to_owned())).transpose()?,
        },
        receipt: row
            .get::<_, Option<String>>(22)?
            .map(|s| {
                serde_json::from_str(&s).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        22,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .transpose()?,
        envelope_seq: read_unsigned(row, 25)?,
        created_at: row.get(23)?,
        updated_at: row.get(24)?,
    })
}
fn load(conn: &rusqlite::Connection, key: EnvelopeKey) -> rusqlite::Result<Option<SenderRecord>> {
    let epoch = i64::try_from(key.recipient_key_epoch)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    conn.query_row(
        &LOAD_SQL.replace("{COLUMNS}", COLUMNS),
        params![
            key.logical_message_id.to_string(),
            key.recipient_device_id.to_string(),
            epoch
        ],
        read_row,
    )
    .optional()
}

const LOAD_SQL: &str = concat!(
    "SELECT {COLUMNS} FROM comm_sender_transport WHERE logical_message_id=?1 AND ",
    "recipient_device_id=?2 AND recipient_key_epoch=?3",
);

const TERMINAL_SQL: &str = concat!(
    "SELECT EXISTS(SELECT 1 FROM comm_sender_transport WHERE logical_message_id=?1 ",
    "AND receipt IS NOT NULL)",
);

const PRIOR_SQL: &str = concat!(
    "SELECT {COLUMNS} FROM comm_sender_transport WHERE logical_message_id=?1 ORDER BY ",
    "envelope_seq DESC LIMIT 1",
);

const DEVICE_EPOCH_SQL: &str = concat!(
    "SELECT MAX(recipient_key_epoch) FROM comm_sender_transport WHERE ",
    "logical_message_id=?1 AND recipient_device_id=?2",
);

const INSERT_SQL: &str = concat!(
    "INSERT INTO comm_sender_transport (namespace, logical_message_id, ",
    "outbound_note_id, kind, slug, credential_ref, recipient_address, ",
    "protocol_version, sender_agent_id, recipient_agent_id, recipient_device_id, ",
    "recipient_key_epoch, contact_generation, sender_key_epoch, ",
    "recipient_key_fingerprint, enc, ",
    "ciphertext,state,created_at,updated_at,envelope_seq) VALUES ",
    "(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17, ",
    "'pending',?18,?18,?19)",
);

const PENDING_SQL: &str = concat!(
    "SELECT {COLUMNS} FROM comm_sender_transport AS t WHERE namespace=?1 AND kind=?2 ",
    "AND slug=?3 AND state='pending' AND hold_reason IS NULL AND (next_retry_at IS ",
    "NULL OR next_retry_at<=?4) AND NOT EXISTS(SELECT 1 FROM comm_sender_transport AS ",
    "done WHERE done.logical_message_id=t.logical_message_id AND done.receipt IS NOT ",
    "NULL) AND envelope_seq=(SELECT MAX(envelope_seq) FROM comm_sender_transport ",
    "WHERE logical_message_id=t.logical_message_id) ORDER BY ",
    "created_at,logical_message_id LIMIT ?5",
);

const FAILURE_SQL: &str = concat!(
    "UPDATE comm_sender_transport SET ",
    "state=?4,attempt_count=?5,next_retry_at=?6,last_failure_class=?7,updated_at=?8 ",
    "WHERE logical_message_id=?1 AND recipient_device_id=?2 AND ",
    "recipient_key_epoch=?3",
);

const HOLD_SQL: &str = concat!(
    "UPDATE comm_sender_transport SET hold_reason=?4,updated_at=?5,",
    "policy_mode=?6,policy_revision=?7 WHERE ",
    "logical_message_id=?1 AND recipient_device_id=?2 AND recipient_key_epoch=?3",
);

const RECEIPT_SQL: &str = concat!(
    "UPDATE comm_sender_transport SET ",
    "state=?4,receipt=?5,next_retry_at=NULL,hold_reason=NULL,",
    "policy_mode=NULL,policy_revision=NULL,updated_at=?6 WHERE ",
    "logical_message_id=?1 AND recipient_device_id=?2 AND recipient_key_epoch=?3",
);

/// Store uses the note writer's transaction/queue routing and bounded pooled readers.
pub struct SenderTransportStore {
    notes: SqlNoteStore,
}
impl SenderTransportStore {
    pub fn new(pool: Arc<ConnectionPool>) -> Self {
        Self {
            notes: SqlNoteStore::new(pool, false),
        }
    }
    pub async fn get(&self, key: EnvelopeKey) -> StorageResult<Option<SenderRecord>> {
        self.notes
            .with_reader("sender_transport_get", move |conn| load(conn, key))
            .await
    }
    /// Exact retries reuse the record. Only confirmed key-change operations
    /// may create another envelope.
    pub async fn create(
        &self,
        envelope: SenderEnvelope,
        confirmed_key_change: bool,
    ) -> StorageResult<SenderRecord> {
        envelope.validate()?;
        self.notes
            .with_writer_tx_storage("sender_transport_create", move |conn| {
                let op = "sender_transport_create";
                if let Some(existing) = load(conn, envelope.key()).map_err(|e| map_err(e, op))? {
                    if confirmed_key_change {
                        return Err(invalid(
                            "confirmed re-encryption requires a new key identity",
                        ));
                    }
                    if existing.envelope != envelope {
                        return Err(invalid("envelope_conflict"));
                    }
                    return Ok(existing);
                }
                let terminal: bool = conn
                    .query_row(
                        TERMINAL_SQL,
                        [envelope.logical_message_id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(|e| map_err(e, op))?;
                if terminal {
                    return Err(invalid("logical message already has a recipient receipt"));
                }
                let prior = conn
                    .query_row(
                        &PRIOR_SQL.replace("{COLUMNS}", COLUMNS),
                        [envelope.logical_message_id.to_string()],
                        read_row,
                    )
                    .optional()
                    .map_err(|e| map_err(e, op))?;
                let envelope_seq = if let Some(prior) = prior {
                    if !confirmed_key_change
                        || prior.state != TransportState::Pending
                        || prior.hold_reason != Some(HoldReason::RecipientKeyChanged)
                    {
                        return Err(invalid(
                            "new envelope requires confirmed recipient key change",
                        ));
                    }
                    let previous_epoch: Option<u64> = conn
                        .query_row(
                            DEVICE_EPOCH_SQL,
                            params![
                                envelope.logical_message_id.to_string(),
                                envelope.recipient_device_id.to_string()
                            ],
                            |row| read_optional_unsigned(row, 0),
                        )
                        .map_err(|e| map_err(e, op))?;
                    if previous_epoch.is_some_and(|epoch| envelope.recipient_key_epoch <= epoch) {
                        return Err(invalid("same device key epoch must increase"));
                    }
                    let a = &prior.envelope;
                    let b = &envelope;
                    if a.namespace != b.namespace
                        || a.outbound_note_id != b.outbound_note_id
                        || a.kind != b.kind
                        || a.slug != b.slug
                        || a.sender_agent_id != b.sender_agent_id
                        || a.recipient_agent_id != b.recipient_agent_id
                        || a.recipient_address != b.recipient_address
                    {
                        return Err(invalid("logical message identity cannot change"));
                    }
                    prior
                        .envelope_seq
                        .checked_add(1)
                        .filter(|seq| *seq <= i64::MAX as u64)
                        .ok_or_else(|| invalid("envelope sequence exhausted"))?
                } else {
                    if confirmed_key_change {
                        return Err(invalid("no prior envelope to re-encrypt"));
                    }
                    1
                };
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    INSERT_SQL,
                    params![
                        envelope.namespace,
                        envelope.logical_message_id.to_string(),
                        envelope.outbound_note_id.to_string(),
                        envelope.kind,
                        envelope.slug,
                        envelope.credential_ref,
                        envelope.recipient_address,
                        envelope.protocol_version,
                        envelope.sender_agent_id,
                        envelope.recipient_agent_id,
                        envelope.recipient_device_id.to_string(),
                        sql_integer(envelope.recipient_key_epoch)?,
                        sql_integer(envelope.contact_generation)?,
                        sql_integer(envelope.sender_key_epoch)?,
                        envelope.recipient_key_fingerprint,
                        envelope.enc,
                        envelope.ciphertext,
                        now,
                        sql_integer(envelope_seq)?
                    ],
                )
                .map_err(|e| map_err(e, op))?;
                load(conn, envelope.key())
                    .map_err(|e| map_err(e, op))?
                    .ok_or_else(|| invalid("inserted record disappeared"))
            })
            .await
    }
    /// Only due, unheld rows of the exact route and highest local envelope
    /// sequence are submitted automatically.
    pub async fn list_pending(
        &self,
        namespace: &str,
        kind: &str,
        slug: &str,
        now: i64,
        limit: u32,
    ) -> StorageResult<Vec<SenderRecord>> {
        let (namespace, kind, slug) = (namespace.to_owned(), kind.to_owned(), slug.to_owned());
        self.notes
            .with_reader("sender_transport_pending", move |conn| {
                let mut stmt = conn.prepare(&PENDING_SQL.replace("{COLUMNS}", COLUMNS))?;
                let rows = stmt
                    .query_map(
                        params![namespace, kind, slug, now, limit.min(1000)],
                        read_row,
                    )?
                    .collect();
                rows
            })
            .await
    }
    pub async fn record_failure(
        &self,
        key: EnvelopeKey,
        class: FailureClass,
        next_retry_at: Option<i64>,
    ) -> StorageResult<()> {
        self.notes
            .with_writer_tx_storage("sender_transport_failure", move |conn| {
                let op = "sender_transport_failure";
                let row = load(conn, key)
                    .map_err(|e| map_err(e, op))?
                    .ok_or_else(|| invalid("unknown sender record"))?;
                if row.state != TransportState::Pending {
                    return Err(invalid("sender record is not pending"));
                }
                let state = if class == FailureClass::Permanent {
                    TransportState::Failed
                } else {
                    TransportState::Pending
                };
                let attempts = if class == FailureClass::Authentication {
                    row.attempt_count
                } else {
                    row.attempt_count.saturating_add(1).min(i64::MAX as u64)
                };
                let retry = if class == FailureClass::Transient {
                    next_retry_at
                } else {
                    None
                };
                conn.execute(
                    FAILURE_SQL,
                    params![
                        key.logical_message_id.to_string(),
                        key.recipient_device_id.to_string(),
                        sql_integer(key.recipient_key_epoch)?,
                        encode(&state),
                        sql_integer(attempts)?,
                        retry,
                        encode(&class),
                        chrono::Utc::now().timestamp_micros()
                    ],
                )
                .map_err(|e| map_err(e, op))?;
                Ok(())
            })
            .await
    }
    /// Release credit or policy holds explicitly. Key-change holds release by creating a confirmed
    /// new envelope. Policy holds require the evaluated mode and revision in the reason;
    /// every other reason, including release (`None`), clears both policy columns.
    pub async fn hold(&self, key: EnvelopeKey, reason: Option<HoldReason>) -> StorageResult<()> {
        self.notes
            .with_writer_tx_storage("sender_transport_hold", move |conn| {
                let op = "sender_transport_hold";
                let row = load(conn, key)
                    .map_err(|e| map_err(e, op))?
                    .ok_or_else(|| invalid("unknown sender record"))?;
                if row.state != TransportState::Pending {
                    return Err(invalid("sender record is not pending"));
                }
                if row.hold_reason == Some(HoldReason::RecipientKeyChanged)
                    && reason != Some(HoldReason::RecipientKeyChanged)
                {
                    return Err(invalid("key change requires confirmed re-encryption"));
                }
                let (policy_mode, policy_revision) = match reason {
                    Some(HoldReason::PolicyDenied { mode, revision }) => {
                        (Some(encode(&mode)), Some(sql_integer(revision)?))
                    }
                    _ => (None, None),
                };
                conn.execute(
                    HOLD_SQL,
                    params![
                        key.logical_message_id.to_string(),
                        key.recipient_device_id.to_string(),
                        sql_integer(key.recipient_key_epoch)?,
                        reason.map(|r| encode(&r)),
                        chrono::Utc::now().timestamp_micros(),
                        policy_mode,
                        policy_revision
                    ],
                )
                .map_err(|e| map_err(e, op))?;
                Ok(())
            })
            .await
    }
    /// Caller verifies the signature before reaching this store. Binding checks repeat inside
    /// the transaction.
    pub async fn accept_receipt(
        &self,
        key: EnvelopeKey,
        state: TransportState,
        receipt: Value,
    ) -> StorageResult<()> {
        self.notes
            .with_writer_tx_storage("sender_transport_receipt", move |conn| {
                let op = "sender_transport_receipt";
                let row = load(conn, key)
                    .map_err(|e| map_err(e, op))?
                    .ok_or_else(|| invalid("unknown sender record"))?;
                let disposition = match state {
                    TransportState::RecipientStored => "stored",
                    TransportState::RecipientQuarantined => "quarantined",
                    _ => return Err(invalid("receipt target is not a recipient outcome")),
                };
                if receipt.get("disposition").and_then(Value::as_str) != Some(disposition) {
                    return Err(invalid("receipt disposition mismatch"));
                }
                let binding = receipt
                    .get("binding")
                    .ok_or_else(|| invalid("missing receipt binding"))?;
                let e = &row.envelope;
                let expected = [
                    ("protocol_version", serde_json::json!(e.protocol_version)),
                    (
                        "logical_message_id",
                        serde_json::json!(e.logical_message_id),
                    ),
                    ("sender_agent_id", serde_json::json!(e.sender_agent_id)),
                    (
                        "recipient_agent_id",
                        serde_json::json!(e.recipient_agent_id),
                    ),
                    (
                        "recipient_device_id",
                        serde_json::json!(e.recipient_device_id),
                    ),
                    (
                        "recipient_key_epoch",
                        serde_json::json!(e.recipient_key_epoch),
                    ),
                    (
                        "contact_generation",
                        serde_json::json!(e.contact_generation),
                    ),
                ];
                for (field, value) in expected {
                    if binding.get(field) != Some(&value) {
                        return Err(invalid("receipt binding mismatch"));
                    }
                }
                let attempt = binding
                    .get("delivery_attempt_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("missing delivery attempt"))?;
                if Uuid::parse_str(attempt)
                    .ok()
                    .map(|id| id.to_string())
                    .as_deref()
                    != Some(attempt)
                {
                    return Err(invalid("invalid delivery attempt"));
                }
                if let Some(accepted) = row.receipt {
                    if accepted != receipt || row.state != state {
                        return Err(invalid("receipt_conflict"));
                    }
                    return Ok(());
                }
                conn.execute(
                    RECEIPT_SQL,
                    params![
                        key.logical_message_id.to_string(),
                        key.recipient_device_id.to_string(),
                        sql_integer(key.recipient_key_epoch)?,
                        encode(&state),
                        receipt.to_string(),
                        chrono::Utc::now().timestamp_micros()
                    ],
                )
                .map_err(|e| map_err(e, op))?;
                Ok(())
            })
            .await
    }
}
#[cfg(test)]
mod tests;
