//! Atomic recipient message, replay, quarantine and acknowledgement persistence.
use super::{assign_note_seq, map_err, SqlNoteStore};
use crate::pool::ConnectionPool;
use khive_storage::{Note, StorageCapability, StorageError, StorageResult};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipientDisposition {
    Stored,
    Quarantined,
}

/// Closed class of terminal acknowledgement refusals from ADR-105 A.9.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcknowledgementRetirementReason {
    PermanentTransport,
}
impl AcknowledgementRetirementReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::PermanentTransport => "permanent_transport",
        }
    }
}

/// Unsigned binding and disposition read from the durable acknowledgement journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AckJournalEntry {
    pub delivery_attempt_id: Uuid,
    pub binding: Value,
    pub disposition: RecipientDisposition,
    pub attempt_count: u64,
    pub not_before: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}
impl RecipientDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Quarantined => "quarantined",
        }
    }
    fn parse(s: &str) -> StorageResult<Self> {
        match s {
            "stored" => Ok(Self::Stored),
            "quarantined" => Ok(Self::Quarantined),
            _ => Err(invalid("invalid stored disposition")),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineReason {
    InvalidPlaintext,
    InvalidMessage,
    PolicyRejected,
}
impl QuarantineReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidPlaintext => "invalid_plaintext",
            Self::InvalidMessage => "invalid_message",
            Self::PolicyRejected => "policy_rejected",
        }
    }
    fn parse(s: &str) -> StorageResult<Self> {
        match s {
            "invalid_plaintext" => Ok(Self::InvalidPlaintext),
            "invalid_message" => Ok(Self::InvalidMessage),
            "policy_rejected" => Ok(Self::PolicyRejected),
            _ => Err(invalid("invalid stored quarantine reason")),
        }
    }
}

/// ADR-105 A.8 Receiving, step 3: the local quarantine bound this client sets,
/// per local recipient agent, for quarantined items other than policy-refused
/// ones. Past it the oldest such item is dropped and reported.
pub const LOCAL_QUARANTINE_BOUND: usize = 512;

/// ADR-105 A.8 Receiving, step 4: policy-refused items are not under the local
/// quarantine bound. Each sender has its own bound, past which the oldest of
/// that sender's policy-refused items is dropped and reported, so one sender's
/// refusals never evict another's.
pub const POLICY_REFUSED_PER_SENDER_BOUND: usize = 64;

#[derive(Clone, Debug)]
pub struct QuarantineRecord {
    pub reason: QuarantineReason,
    pub delivery_item: Vec<u8>,
    /// The parsed plaintext object, present exactly for a policy refusal
    /// (ADR-105 A.8 Receiving, step 4).
    pub parsed_plaintext: Option<Value>,
}
/// Internal storage contract. Runtime supplies authenticated identity and a
/// validated note; this structure is not a wire-ingest parameter.
#[derive(Clone, Debug)]
pub struct RecipientCommit {
    /// The message note, present exactly for a stored disposition. A
    /// quarantined delivery writes no message note.
    pub note: Option<Note>,
    /// Local recipient actor from the trusted enrollment binding.
    pub recipient_actor: String,
    pub binding: Value,
    pub sender_agent_id: String,
    pub logical_message_id: Uuid,
    pub delivery_attempt_id: Uuid,
    pub disposition: RecipientDisposition,
    pub quarantine: Option<QuarantineRecord>,
    pub in_reply_to: Option<Uuid>,
    pub correlation: Option<String>,
}
/// A quarantined item dropped to keep a quarantine bound. Its replay identity
/// stays claimed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvictedQuarantine {
    pub sender_agent_id: String,
    pub logical_message_id: Uuid,
    pub reason: QuarantineReason,
}
#[derive(Debug)]
pub struct RecipientCommitResult {
    /// The message note of a stored identity; a quarantined identity has none.
    pub note_id: Option<Uuid>,
    pub disposition: RecipientDisposition,
    pub created: bool,
    /// Present only for a newly committed note, for best-effort indexing.
    pub note: Option<Note>,
    /// Quarantined items this commit dropped to keep a quarantine bound.
    pub evicted: Vec<EvictedQuarantine>,
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: "recipient_transport".into(),
        message: message.into(),
    }
}

const REPLAY_SQL: &str = include_str!("../../../../sql/comm-recipient-replay-select.sql");

// An outbox transport row alone survives deletion of its message note. A
// parent proves reply status only while that exact outbound note is live and
// belongs to this recipient, addressed to the authenticated sender.
const OUTBOUND_PARENT_SQL: &str =
    include_str!("../../../../sql/comm-recipient-live-outbound-parent-exists.sql");

const CORRELATION_SQL: &str =
    include_str!("../../../../sql/comm-recipient-correlated-thread-select.sql");

fn correlation_match_values(correlation: &str) -> ([Option<String>; 9], String) {
    let raw = correlation.trim();
    let mut spellings = std::array::from_fn(|_| None);
    spellings[0] = Some(raw.to_owned());
    let Ok(root) = Uuid::parse_str(raw) else {
        return (spellings, correlation.to_owned());
    };
    spellings[1] = Some(root.as_hyphenated().to_string());
    spellings[2] = Some(root.simple().to_string());
    spellings[3] = Some(root.braced().to_string());
    spellings[4] = Some(root.urn().to_string());
    spellings[5] = Some(format!("{:X}", root.as_hyphenated()));
    spellings[6] = Some(format!("{:X}", root.simple()));
    spellings[7] = Some(format!("{:X}", root.braced()));
    spellings[8] = Some(format!("{:X}", root.urn()));
    (spellings, root.to_string())
}

const INSERT_NOTE_SQL: &str =
    include_str!("../../../../sql/comm-recipient-message-note-insert.sql");

const INSERT_REPLAY_SQL: &str = include_str!("../../../../sql/comm-recipient-replay-insert.sql");

const QUARANTINE_SQL: &str = include_str!("../../../../sql/comm-recipient-quarantine-insert.sql");

const LOCAL_BOUND_COUNT_SQL: &str =
    include_str!("../../../../sql/comm-recipient-local-quarantine-count.sql");
const LOCAL_BOUND_OLDEST_SQL: &str =
    include_str!("../../../../sql/comm-recipient-local-quarantine-oldest-select.sql");
const SENDER_BOUND_COUNT_SQL: &str =
    include_str!("../../../../sql/comm-recipient-sender-quarantine-count.sql");
const SENDER_BOUND_OLDEST_SQL: &str =
    include_str!("../../../../sql/comm-recipient-sender-quarantine-oldest-select.sql");
const DELETE_QUARANTINE_SQL: &str =
    include_str!("../../../../sql/comm-recipient-quarantine-delete.sql");

/// Make room for one more quarantined item of `reason` under its bound by
/// dropping the oldest items in the same bound, oldest `created_at` first,
/// then by key. Only the quarantine rows go: every replay identity stays claimed.
fn evict_to_bound(
    conn: &rusqlite::Connection,
    recipient_agent_id: &str,
    sender_agent_id: &str,
    reason: QuarantineReason,
    op: &'static str,
) -> StorageResult<Vec<EvictedQuarantine>> {
    let row = |r: &rusqlite::Row<'_>| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    };
    let oldest: Vec<(String, String, String)> = if reason == QuarantineReason::PolicyRejected {
        let held: i64 = conn
            .query_row(
                SENDER_BOUND_COUNT_SQL,
                params![recipient_agent_id, sender_agent_id],
                |r| r.get(0),
            )
            .map_err(|e| map_err(e, op))?;
        let excess = held + 1 - POLICY_REFUSED_PER_SENDER_BOUND as i64;
        if excess <= 0 {
            return Ok(Vec::new());
        }
        let mut stmt = conn
            .prepare(SENDER_BOUND_OLDEST_SQL)
            .map_err(|e| map_err(e, op))?;
        let rows = stmt
            .query_map(params![recipient_agent_id, sender_agent_id, excess], row)
            .map_err(|e| map_err(e, op))?;
        rows.collect::<Result<_, _>>().map_err(|e| map_err(e, op))?
    } else {
        let held: i64 = conn
            .query_row(LOCAL_BOUND_COUNT_SQL, params![recipient_agent_id], |r| {
                r.get(0)
            })
            .map_err(|e| map_err(e, op))?;
        let excess = held + 1 - LOCAL_QUARANTINE_BOUND as i64;
        if excess <= 0 {
            return Ok(Vec::new());
        }
        let mut stmt = conn
            .prepare(LOCAL_BOUND_OLDEST_SQL)
            .map_err(|e| map_err(e, op))?;
        let rows = stmt
            .query_map(params![recipient_agent_id, excess], row)
            .map_err(|e| map_err(e, op))?;
        rows.collect::<Result<_, _>>().map_err(|e| map_err(e, op))?
    };
    oldest
        .into_iter()
        .map(|(sender, logical, reason)| {
            conn.execute(DELETE_QUARANTINE_SQL, params![sender, logical])
                .map_err(|e| map_err(e, op))?;
            Ok(EvictedQuarantine {
                sender_agent_id: sender,
                logical_message_id: Uuid::parse_str(&logical)
                    .map_err(|_| invalid("invalid quarantined logical message id"))?,
                reason: QuarantineReason::parse(&reason)?,
            })
        })
        .collect()
}

const ACK_LOOKUP_SQL: &str = include_str!("../../../../sql/comm-ack-binding-select.sql");

const ACK_DUE_SQL: &str = include_str!("../../../../sql/comm-ack-due-select.sql");

const ACK_FINISH_SQL: &str = include_str!("../../../../sql/comm-ack-finish-update.sql");

const ACK_FAILED_TRY_SQL: &str = include_str!("../../../../sql/comm-ack-failed-try-update.sql");

const ACK_RETIRE_SQL: &str = include_str!("../../../../sql/comm-ack-retire-update.sql");

fn read_ack_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<AckJournalEntry> {
    let attempt: String = row.get(0)?;
    let delivery_attempt_id = Uuid::parse_str(&attempt).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let binding: String = row.get(1)?;
    let binding = serde_json::from_str(&binding).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let disposition: String = row.get(2)?;
    let disposition = serde_json::from_value(Value::String(disposition)).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let attempt_count = u64::try_from(row.get::<_, i64>(3)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    Ok(AckJournalEntry {
        delivery_attempt_id,
        binding,
        disposition,
        attempt_count,
        not_before: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

const INSERT_ACK_SQL: &str = include_str!("../../../../sql/comm-ack-insert.sql");

struct AckIdentity<'a> {
    binding: &'a Value,
    sender_agent_id: &'a str,
    logical_message_id: Uuid,
    delivery_attempt_id: Uuid,
}

fn record_ack(
    conn: &rusqlite::Connection,
    identity: AckIdentity<'_>,
    disposition: RecipientDisposition,
    now: i64,
    op: &'static str,
) -> StorageResult<()> {
    let prior_ack: Option<(String, String)> = conn
        .query_row(
            ACK_LOOKUP_SQL,
            [identity.delivery_attempt_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|error| map_err(error, op))?;
    if let Some((prior_binding, prior_disposition)) = prior_ack {
        let prior_binding: Value = serde_json::from_str(&prior_binding)
            .map_err(|_| invalid("invalid stored ack binding"))?;
        if prior_binding != *identity.binding || prior_disposition != disposition.as_str() {
            return Err(invalid("ack attempt binding conflict"));
        }
    } else {
        conn.execute(
            INSERT_ACK_SQL,
            params![
                identity.delivery_attempt_id.to_string(),
                identity.sender_agent_id,
                identity.logical_message_id.to_string(),
                identity.binding.to_string(),
                disposition.as_str(),
                now
            ],
        )
        .map_err(|error| map_err(error, op))?;
    }
    Ok(())
}

pub struct RecipientTransportStore {
    notes: SqlNoteStore,
}
impl RecipientTransportStore {
    pub fn new(pool: Arc<ConnectionPool>) -> Self {
        Self {
            notes: SqlNoteStore::new(pool, false),
        }
    }

    /// Pending entries at or before `now` (UTC microseconds), oldest first.
    /// Pages are bounded to 1,000 entries; a zero limit returns no entries.
    pub async fn list_due_acknowledgements(
        &self,
        now: i64,
        limit: usize,
    ) -> StorageResult<Vec<AckJournalEntry>> {
        let limit = limit.min(1000) as i64;
        self.notes
            .with_reader("recipient_acknowledgement_due", move |conn| {
                let mut statement = conn.prepare(ACK_DUE_SQL)?;
                let entries = statement
                    .query_map(params![now, limit], read_ack_entry)?
                    .collect();
                entries
            })
            .await
    }

    /// Finish a pending entry; terminal or absent entries return `false` unchanged.
    pub async fn finish_acknowledgement(&self, delivery_attempt_id: Uuid) -> StorageResult<bool> {
        self.notes
            .with_writer_tx_storage("recipient_acknowledgement_finish", move |conn| {
                let changed = conn
                    .execute(
                        ACK_FINISH_SQL,
                        params![
                            delivery_attempt_id.to_string(),
                            chrono::Utc::now().timestamp_micros()
                        ],
                    )
                    .map_err(|error| map_err(error, "recipient_acknowledgement_finish"))?;
                Ok(changed != 0)
            })
            .await
    }

    /// Persist one failed try and its next eligible UTC-microsecond deadline.
    /// The integer constraint aborts a counter overflow without changing the row.
    pub async fn record_acknowledgement_failed_try(
        &self,
        delivery_attempt_id: Uuid,
        not_before: i64,
    ) -> StorageResult<bool> {
        self.notes
            .with_writer_tx_storage("recipient_acknowledgement_failed_try", move |conn| {
                let changed = conn
                    .execute(
                        ACK_FAILED_TRY_SQL,
                        params![
                            delivery_attempt_id.to_string(),
                            not_before,
                            chrono::Utc::now().timestamp_micros()
                        ],
                    )
                    .map_err(|error| map_err(error, "recipient_acknowledgement_failed_try"))?;
                Ok(changed != 0)
            })
            .await
    }

    /// Retire a pending acknowledgement without touching its message or replay claim.
    pub async fn retire_acknowledgement(
        &self,
        delivery_attempt_id: Uuid,
        reason: AcknowledgementRetirementReason,
    ) -> StorageResult<bool> {
        self.notes
            .with_writer_tx_storage("recipient_acknowledgement_retire", move |conn| {
                let changed = conn
                    .execute(
                        ACK_RETIRE_SQL,
                        params![
                            delivery_attempt_id.to_string(),
                            reason.as_str(),
                            chrono::Utc::now().timestamp_micros()
                        ],
                    )
                    .map_err(|error| map_err(error, "recipient_acknowledgement_retire"))?;
                Ok(changed != 0)
            })
            .await
    }
    /// Answer an already committed logical message before its plaintext is
    /// interpreted again. The authenticated caller supplies the current local
    /// actor and binding; a hit writes only the new attempt's ack journal row.
    /// A miss makes no claim: `commit` rechecks replay under its own writer
    /// transaction after the new delivery has been validated.
    pub async fn ack_if_replayed(
        &self,
        binding: Value,
        recipient_actor: &str,
    ) -> StorageResult<Option<RecipientCommitResult>> {
        let (sender_agent_id, recipient_agent_id, logical_message_id, delivery_attempt_id) = {
            let string_field = |name: &str| {
                binding
                    .get(name)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("invalid replay receipt binding"))
            };
            let logical_message_id = Uuid::parse_str(&string_field("logical_message_id")?)
                .map_err(|_| invalid("invalid replay logical message id"))?;
            let delivery_attempt_id = Uuid::parse_str(&string_field("delivery_attempt_id")?)
                .map_err(|_| invalid("invalid replay delivery attempt id"))?;
            (
                string_field("sender_agent_id")?,
                string_field("recipient_agent_id")?,
                logical_message_id,
                delivery_attempt_id,
            )
        };
        let recipient_actor = recipient_actor.to_owned();
        self.notes
            .with_writer_tx_storage("recipient_transport_replay", move |conn| {
                let op = "recipient_transport_replay";
                let prior: Option<(Option<String>, String, String, String)> = conn
                    .query_row(
                        REPLAY_SQL,
                        params![sender_agent_id, logical_message_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()
                    .map_err(|error| map_err(error, op))?;
                let Some((id, disposition, prior_recipient, prior_actor)) = prior else {
                    return Ok(None);
                };
                if prior_recipient != recipient_agent_id || prior_actor != recipient_actor {
                    return Err(invalid("replay recipient binding changed"));
                }
                let note_id = id
                    .map(|id| Uuid::parse_str(&id).map_err(|_| invalid("invalid replay note id")))
                    .transpose()?;
                let disposition = RecipientDisposition::parse(&disposition)?;
                record_ack(
                    conn,
                    AckIdentity {
                        binding: &binding,
                        sender_agent_id: &sender_agent_id,
                        logical_message_id,
                        delivery_attempt_id,
                    },
                    disposition,
                    chrono::Utc::now().timestamp_micros(),
                    op,
                )?;
                Ok(Some(RecipientCommitResult {
                    note_id,
                    disposition,
                    created: false,
                    note: None,
                    evicted: Vec::new(),
                }))
            })
            .await
    }

    /// Commit all recipient effects together, or none. A replay keeps its first
    /// disposition and creates only the acknowledgement for a new attempt.
    ///
    /// A stored disposition writes the message note. A quarantined one writes
    /// the quarantine record and no message note (ADR-105 A.8 Receiving, step
    /// 4), after dropping the oldest item of its quarantine bound when that
    /// bound is full. Dropped items are returned in `evicted` and logged; their
    /// replay identities stay claimed.
    pub async fn commit(&self, mut input: RecipientCommit) -> StorageResult<RecipientCommitResult> {
        match input.disposition {
            RecipientDisposition::Quarantined => {
                if input.quarantine.is_none() {
                    return Err(invalid("quarantine requires replay bytes"));
                }
                if input.note.is_some() {
                    return Err(invalid("quarantined delivery cannot carry a message note"));
                }
            }
            RecipientDisposition::Stored => {
                if input.quarantine.is_some() {
                    return Err(invalid("stored message cannot carry quarantine bytes"));
                }
                if input.note.is_none() {
                    return Err(invalid("stored message requires a message note"));
                }
            }
        }
        if let Some(q) = &input.quarantine {
            if q.delivery_item.len() > 98_304
                || !serde_json::from_slice::<Value>(&q.delivery_item).is_ok_and(|v| v.is_object())
            {
                return Err(invalid(
                    "quarantine delivery item must be one JSON object within 98304 bytes",
                ));
            }
            match (q.reason, &q.parsed_plaintext) {
                (QuarantineReason::PolicyRejected, Some(plaintext)) if plaintext.is_object() => {}
                (QuarantineReason::PolicyRejected, _) => {
                    return Err(invalid(
                        "policy-refused quarantine requires its parsed plaintext object",
                    ));
                }
                (_, Some(_)) => {
                    return Err(invalid(
                        "only a policy-refused quarantine keeps parsed plaintext",
                    ));
                }
                (_, None) => {}
            }
        }
        if input.binding.get("sender_agent_id").and_then(Value::as_str)
            != Some(input.sender_agent_id.as_str())
            || input.binding.get("logical_message_id")
                != Some(&serde_json::json!(input.logical_message_id))
            || input.binding.get("delivery_attempt_id")
                != Some(&serde_json::json!(input.delivery_attempt_id))
        {
            return Err(invalid("receipt key mismatch"));
        }
        let recipient = input
            .binding
            .get("recipient_agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing recipient agent"))?
            .to_owned();
        if input.recipient_actor.trim().is_empty() {
            return Err(invalid("missing recipient actor"));
        }
        let actor = input.recipient_actor.clone();
        if let Some(note) = &input.note {
            if note
                .properties
                .as_ref()
                .and_then(|p| p.get("to_actor"))
                .and_then(Value::as_str)
                != Some(actor.as_str())
            {
                return Err(invalid(
                    "message note is not addressed to the recipient actor",
                ));
            }
        }
        let correlated_thread = if let (Some(correlation), Some(note)) =
            (input.correlation.as_deref(), input.note.as_ref())
        {
            if let Some(sender) = note
                .properties
                .as_ref()
                .and_then(|p| p.get("from_actor"))
                .and_then(Value::as_str)
            {
                let (spellings, id) = correlation_match_values(correlation);
                let namespace = note.namespace.clone();
                let sender = sender.to_owned();
                let actor_for_query = actor.clone();
                let correlation = correlation.to_owned();
                let matched: Option<(String, Option<String>)> = self
                    .notes
                    .with_reader("recipient_transport_correlation", move |conn| {
                        conn.query_row(
                            CORRELATION_SQL,
                            params![
                                namespace,
                                sender,
                                actor_for_query,
                                correlation,
                                &spellings[0],
                                &spellings[1],
                                &spellings[2],
                                &spellings[3],
                                &spellings[4],
                                &spellings[5],
                                &spellings[6],
                                &spellings[7],
                                &spellings[8],
                                id
                            ],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()
                    })
                    .await?;
                matched
                    .map(|(id, thread)| {
                        let matched_id = Uuid::parse_str(&id)
                            .map_err(|_| invalid("invalid correlated note id"))?;
                        Ok(thread
                            .and_then(|s| Uuid::parse_str(&s).ok())
                            .unwrap_or(matched_id))
                    })
                    .transpose()?
            } else {
                None
            }
        } else {
            None
        };
        let result = self
            .notes
            .with_writer_tx_storage("recipient_transport_commit", move |conn| {
                let op = "recipient_transport_commit";
                let prior: Option<(Option<String>, String, String, String)> = conn
                    .query_row(
                        REPLAY_SQL,
                        params![input.sender_agent_id, input.logical_message_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                    .map_err(|e| map_err(e, op))?;
                let now = chrono::Utc::now().timestamp_micros();
                let mut evicted = Vec::new();
                let (note_id, disposition, created) =
                    if let Some((id, disposition, prior_recipient, prior_actor)) = prior {
                        if prior_recipient != recipient || prior_actor != actor {
                            return Err(invalid("replay recipient binding changed"));
                        }
                        (
                            id.map(|id| {
                                Uuid::parse_str(&id).map_err(|_| invalid("invalid replay note id"))
                            })
                            .transpose()?,
                            RecipientDisposition::parse(&disposition)?,
                            false,
                        )
                    } else {
                        let note_id = if let Some(note) = input.note.as_mut() {
                            let props = note
                                .properties
                                .as_mut()
                                .and_then(Value::as_object_mut)
                                .ok_or_else(|| invalid("missing message properties"))?;
                            props
                                .get("from_actor")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid("missing sender actor"))?;
                            if let Some(parent) = input.in_reply_to {
                                let is_reply: bool = conn
                                    .query_row(
                                        OUTBOUND_PARENT_SQL,
                                        params![
                                            note.namespace.as_str(),
                                            parent.to_string(),
                                            recipient.as_str(),
                                            input.sender_agent_id.as_str()
                                        ],
                                        |row| row.get(0),
                                    )
                                    .map_err(|e| map_err(e, op))?;
                                if is_reply {
                                    props.insert(
                                        "message_kind".into(),
                                        Value::String("reply".into()),
                                    );
                                }
                            }
                            let thread = correlated_thread.unwrap_or(note.id);
                            props.insert("thread_id".into(), serde_json::json!(thread));
                            let n = &*note;
                            conn.execute(
                                INSERT_NOTE_SQL,
                                params![
                                    n.id.to_string(),
                                    n.namespace,
                                    n.kind,
                                    n.status,
                                    n.name,
                                    n.content,
                                    n.salience,
                                    n.decay_factor,
                                    n.expires_at,
                                    n.properties.as_ref().map(Value::to_string),
                                    n.created_at,
                                    n.updated_at,
                                    n.deleted_at,
                                    n.key
                                ],
                            )
                            .map_err(|e| map_err(e, op))?;
                            assign_note_seq(conn, &n.id.to_string()).map_err(|e| map_err(e, op))?;
                            Some(n.id)
                        } else {
                            None
                        };
                        conn.execute(
                            INSERT_REPLAY_SQL,
                            params![
                                input.sender_agent_id,
                                input.logical_message_id.to_string(),
                                recipient,
                                actor,
                                note_id.map(|id| id.to_string()),
                                input.disposition.as_str(),
                                now
                            ],
                        )
                        .map_err(|e| map_err(e, op))?;
                        if let Some(q) = &input.quarantine {
                            evicted = evict_to_bound(
                                conn,
                                &recipient,
                                &input.sender_agent_id,
                                q.reason,
                                op,
                            )?;
                            conn.execute(
                                QUARANTINE_SQL,
                                params![
                                    input.sender_agent_id,
                                    input.logical_message_id.to_string(),
                                    recipient,
                                    q.delivery_item,
                                    q.reason.as_str(),
                                    q.parsed_plaintext.as_ref().map(Value::to_string),
                                    now
                                ],
                            )
                            .map_err(|e| map_err(e, op))?;
                        }
                        (note_id, input.disposition, true)
                    };
                // This insertion deliberately shares the message/replay transaction.
                record_ack(
                    conn,
                    AckIdentity {
                        binding: &input.binding,
                        sender_agent_id: &input.sender_agent_id,
                        logical_message_id: input.logical_message_id,
                        delivery_attempt_id: input.delivery_attempt_id,
                    },
                    disposition,
                    now,
                    op,
                )?;
                Ok(RecipientCommitResult {
                    note_id,
                    disposition,
                    created,
                    note: if created { input.note } else { None },
                    evicted,
                })
            })
            .await?;
        for item in &result.evicted {
            tracing::warn!(
                sender_agent_id = %item.sender_agent_id,
                logical_message_id = %item.logical_message_id,
                reason = item.reason.as_str(),
                "quarantine bound reached; dropped the oldest quarantined item"
            );
        }
        Ok(result)
    }
}
#[cfg(test)]
mod tests;
