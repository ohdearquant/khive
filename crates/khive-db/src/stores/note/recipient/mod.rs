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
}
#[derive(Clone, Debug)]
pub struct QuarantineRecord {
    pub reason: QuarantineReason,
    pub delivery_item: Vec<u8>,
}
/// Internal storage contract. Runtime supplies authenticated identity and a
/// validated note; this structure is not a wire-ingest parameter.
#[derive(Clone, Debug)]
pub struct RecipientCommit {
    pub note: Note,
    pub binding: Value,
    pub sender_agent_id: String,
    pub logical_message_id: Uuid,
    pub delivery_attempt_id: Uuid,
    pub disposition: RecipientDisposition,
    pub quarantine: Option<QuarantineRecord>,
    pub correlation: Option<String>,
}
#[derive(Debug)]
pub struct RecipientCommitResult {
    pub note_id: Uuid,
    pub disposition: RecipientDisposition,
    pub created: bool,
    /// Present only for a newly committed note, for best-effort indexing.
    pub note: Option<Note>,
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: "recipient_transport".into(),
        message: message.into(),
    }
}

const REPLAY_SQL: &str = concat!(
    "SELECT note_id,disposition,recipient_agent_id,recipient_actor FROM ",
    "comm_recipient_replay WHERE sender_agent_id=?1 AND logical_message_id=?2",
);

const CORRELATION_SQL: &str = concat!(
    "SELECT COALESCE(json_extract(properties,'$.thread_id'),id) FROM notes WHERE ",
    "namespace=?1 AND kind='message' AND deleted_at IS NULL AND ",
    "((json_extract(properties,'$.from_actor')=?2 AND ",
    "json_extract(properties,'$.to_actor')=?3) OR ",
    "(json_extract(properties,'$.from_actor')=?3 AND ",
    "json_extract(properties,'$.to_actor')=?2)) AND ",
    "(json_extract(properties,'$.external_id')=?4 OR ",
    "json_extract(properties,'$.thread_id')=?4 OR id=?4) ORDER BY created_at,id ",
    "LIMIT 1",
);

const INSERT_NOTE_SQL: &str = concat!(
    "INSERT INTO notes ",
    "(id,namespace,kind,status,name,content,salience,decay_factor,expires_at,",
    "properties,created_at,updated_at,deleted_at,key) ",
    "VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
);

const INSERT_REPLAY_SQL: &str = concat!(
    "INSERT INTO comm_recipient_replay ",
    "(sender_agent_id,logical_message_id,recipient_agent_id,recipient_actor,",
    "note_id,disposition,created_at) ",
    "VALUES (?1,?2,?3,?4,?5,?6,?7)",
);

const QUARANTINE_SQL: &str = concat!(
    "INSERT INTO comm_recipient_quarantine ",
    "(sender_agent_id,logical_message_id,delivery_item,reason,created_at) VALUES ",
    "(?1,?2,?3,?4,?5)",
);

const ACK_LOOKUP_SQL: &str =
    "SELECT binding,disposition FROM comm_ack_work WHERE delivery_attempt_id=?1";

const INSERT_ACK_SQL: &str = concat!(
    "INSERT INTO comm_ack_work ",
    "(delivery_attempt_id,sender_agent_id,logical_message_id,binding,disposition,",
    "created_at,updated_at) ",
    "VALUES (?1,?2,?3,?4,?5,?6,?6)",
);

pub struct RecipientTransportStore {
    notes: SqlNoteStore,
}
impl RecipientTransportStore {
    pub fn new(pool: Arc<ConnectionPool>) -> Self {
        Self {
            notes: SqlNoteStore::new(pool, false),
        }
    }
    /// Commit all recipient effects together, or none. A replay keeps its first
    /// disposition and creates only the acknowledgement for a new attempt.
    pub async fn commit(&self, mut input: RecipientCommit) -> StorageResult<RecipientCommitResult> {
        if input.disposition == RecipientDisposition::Quarantined && input.quarantine.is_none() {
            return Err(invalid("quarantine requires replay bytes"));
        }
        if input.disposition == RecipientDisposition::Stored && input.quarantine.is_some() {
            return Err(invalid("stored message cannot carry quarantine bytes"));
        }
        if let Some(q) = &input.quarantine {
            if q.delivery_item.len() > 98_304
                || !serde_json::from_slice::<Value>(&q.delivery_item).is_ok_and(|v| v.is_object())
            {
                return Err(invalid(
                    "quarantine delivery item must be one JSON object within 98304 bytes",
                ));
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
        let actor = input
            .note
            .properties
            .as_ref()
            .and_then(|p| p.get("to_actor"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("missing recipient actor"))?
            .to_owned();
        self.notes
            .with_writer_tx_storage("recipient_transport_commit", move |conn| {
                let op = "recipient_transport_commit";
                let prior: Option<(String, String, String, String)> = conn
                    .query_row(
                        REPLAY_SQL,
                        params![input.sender_agent_id, input.logical_message_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                    .map_err(|e| map_err(e, op))?;
                let now = chrono::Utc::now().timestamp_micros();
                let (note_id, disposition, created) =
                    if let Some((id, disposition, prior_recipient, prior_actor)) = prior {
                        if prior_recipient != recipient || prior_actor != actor {
                            return Err(invalid("replay recipient binding changed"));
                        }
                        (
                            Uuid::parse_str(&id).map_err(|_| invalid("invalid replay note id"))?,
                            RecipientDisposition::parse(&disposition)?,
                            false,
                        )
                    } else {
                        let props = input
                            .note
                            .properties
                            .as_mut()
                            .and_then(Value::as_object_mut)
                            .ok_or_else(|| invalid("missing message properties"))?;
                        let sender = props
                            .get("from_actor")
                            .and_then(Value::as_str)
                            .ok_or_else(|| invalid("missing sender actor"))?
                            .to_owned();
                        let thread: Option<String> =
                            if let Some(correlation) = input.correlation.as_deref() {
                                conn.query_row(
                                    CORRELATION_SQL,
                                    params![input.note.namespace, sender, actor, correlation],
                                    |r| r.get(0),
                                )
                                .optional()
                                .map_err(|e| map_err(e, op))?
                            } else {
                                None
                            };
                        let thread = thread
                            .and_then(|s| Uuid::parse_str(&s).ok())
                            .unwrap_or(input.note.id);
                        props.insert("thread_id".into(), serde_json::json!(thread));
                        let n = &input.note;
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
                        conn.execute(
                            INSERT_REPLAY_SQL,
                            params![
                                input.sender_agent_id,
                                input.logical_message_id.to_string(),
                                recipient,
                                actor,
                                n.id.to_string(),
                                input.disposition.as_str(),
                                now
                            ],
                        )
                        .map_err(|e| map_err(e, op))?;
                        if let Some(q) = &input.quarantine {
                            conn.execute(
                                QUARANTINE_SQL,
                                params![
                                    input.sender_agent_id,
                                    input.logical_message_id.to_string(),
                                    q.delivery_item,
                                    q.reason.as_str(),
                                    now
                                ],
                            )
                            .map_err(|e| map_err(e, op))?;
                        }
                        (n.id, input.disposition, true)
                    };
                // This insertion deliberately shares the message/replay transaction.
                let prior_ack: Option<(String, String)> = conn
                    .query_row(
                        ACK_LOOKUP_SQL,
                        [input.delivery_attempt_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(|e| map_err(e, op))?;
                if let Some((binding, prior_disposition)) = prior_ack {
                    let binding: Value = serde_json::from_str(&binding)
                        .map_err(|_| invalid("invalid stored ack binding"))?;
                    if binding != input.binding || prior_disposition != disposition.as_str() {
                        return Err(invalid("ack attempt binding conflict"));
                    }
                } else {
                    conn.execute(
                        INSERT_ACK_SQL,
                        params![
                            input.delivery_attempt_id.to_string(),
                            input.sender_agent_id,
                            input.logical_message_id.to_string(),
                            input.binding.to_string(),
                            disposition.as_str(),
                            now
                        ],
                    )
                    .map_err(|e| map_err(e, op))?;
                }
                Ok(RecipientCommitResult {
                    note_id,
                    disposition,
                    created,
                    note: created.then_some(input.note),
                })
            })
            .await
    }
}
#[cfg(test)]
mod tests;
