//! Durable history of confirmed node public keys.
//!
//! Rows are keyed by the full peer identity plus key epoch. A retained pin is
//! never overwritten or deleted, so an old epoch stays readable after a newer
//! one is retained. Public keys are not secret.
use super::{map_err, SqlNoteStore};
use crate::pool::ConnectionPool;
use khive_storage::{StorageCapability, StorageError, StorageResult};
use rusqlite::types::Type;
use rusqlite::{params, OptionalExtension};
use std::sync::Arc;
use uuid::Uuid;

/// Identity of one confirmed key: peer address parts, device, and key epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodePinKey {
    pub namespace: String,
    pub slug: String,
    pub realm: String,
    pub agent_id: Uuid,
    pub device_id: Uuid,
    pub key_epoch: u64,
}

/// A confirmed public-key triple for one identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodePinRecord {
    pub key: NodePinKey,
    pub kem_public_key: [u8; 32],
    pub signing_public_key: [u8; 32],
    pub fingerprint: [u8; 32],
}

const CONFLICT: &str = "node pin identity already has different confirmed keys";

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: "node_pins".into(),
        message: message.into(),
    }
}

/// Epochs are stored as SQLite integers and must fit 1..=u32::MAX.
fn epoch_to_sql(epoch: u64) -> StorageResult<i64> {
    match u32::try_from(epoch) {
        Ok(e) if e >= 1 => Ok(i64::from(e)),
        _ => Err(invalid(
            "node pin key epoch must be between 1 and 4294967295",
        )),
    }
}

const COLUMNS: &str = concat!(
    "namespace, slug, realm, agent_id, device_id, key_epoch, kem_public_key, ",
    "signing_public_key, fingerprint",
);

const LOAD_SQL: &str = concat!(
    "SELECT namespace, slug, realm, agent_id, device_id, key_epoch, kem_public_key, ",
    "signing_public_key, fingerprint FROM comm_node_confirmed_pin WHERE namespace = ?1 ",
    "AND slug = ?2 AND realm = ?3 AND agent_id = ?4 ",
    "AND device_id = ?5 ",
    "AND key_epoch = ?6",
);

fn conversion_error(
    index: usize,
    ty: Type,
    error: Box<dyn std::error::Error + Send + Sync>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, ty, error)
}

fn blob32(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<[u8; 32]> {
    let bytes: Vec<u8> = row.get(index)?;
    let len = bytes.len();
    bytes.try_into().map_err(|_| {
        conversion_error(
            index,
            Type::Blob,
            format!("expected 32 bytes, found {len}").into(),
        )
    })
}

fn uuid_col(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    let text: String = row.get(index)?;
    Uuid::parse_str(&text).map_err(|e| conversion_error(index, Type::Text, Box::new(e)))
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NodePinRecord> {
    let epoch: i64 = row.get(5)?;
    let key_epoch =
        u64::try_from(epoch).map_err(|e| conversion_error(5, Type::Integer, Box::new(e)))?;
    Ok(NodePinRecord {
        key: NodePinKey {
            namespace: row.get(0)?,
            slug: row.get(1)?,
            realm: row.get(2)?,
            agent_id: uuid_col(row, 3)?,
            device_id: uuid_col(row, 4)?,
            key_epoch,
        },
        kem_public_key: blob32(row, 6)?,
        signing_public_key: blob32(row, 7)?,
        fingerprint: blob32(row, 8)?,
    })
}

fn load(
    conn: &rusqlite::Connection,
    key: &NodePinKey,
    epoch: i64,
) -> rusqlite::Result<Option<NodePinRecord>> {
    conn.query_row(
        LOAD_SQL,
        params![
            key.namespace,
            key.slug,
            key.realm,
            key.agent_id.to_string(),
            key.device_id.to_string(),
            epoch
        ],
        read_row,
    )
    .optional()
}

/// Store uses the note writer's transaction routing and bounded pooled readers.
pub struct NodePinStore {
    notes: SqlNoteStore,
}

impl NodePinStore {
    pub fn new(pool: Arc<ConnectionPool>) -> Self {
        Self {
            notes: SqlNoteStore::new(pool, false),
        }
    }

    /// Retain confirmed pins in one transaction. An exact existing payload is a
    /// no-op. Different bytes at an existing identity refuse the whole call and
    /// leave every entry of it unwritten. Nothing is overwritten or deleted.
    pub async fn retain(&self, pins: Vec<NodePinRecord>) -> StorageResult<()> {
        let epochs = pins
            .iter()
            .map(|pin| epoch_to_sql(pin.key.key_epoch))
            .collect::<StorageResult<Vec<_>>>()?;
        if pins.is_empty() {
            return Ok(());
        }
        self.notes
            .with_writer_tx_storage("node_pins_retain", move |conn| {
                let op = "node_pins_retain";
                for (record, epoch) in pins.iter().zip(epochs) {
                    match load(conn, &record.key, epoch).map_err(|e| map_err(e, op))? {
                        Some(existing) if existing == *record => {}
                        Some(_) => return Err(invalid(CONFLICT)),
                        None => {
                            conn.execute(
                                &format!(
                                    "INSERT INTO comm_node_confirmed_pin({COLUMNS}) \
                                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                                ),
                                params![
                                    record.key.namespace,
                                    record.key.slug,
                                    record.key.realm,
                                    record.key.agent_id.to_string(),
                                    record.key.device_id.to_string(),
                                    epoch,
                                    record.kem_public_key.as_slice(),
                                    record.signing_public_key.as_slice(),
                                    record.fingerprint.as_slice()
                                ],
                            )
                            .map_err(|e| map_err(e, op))?;
                        }
                    }
                }
                // Every entry matched an existing row or was inserted.
                Ok(())
            })
            .await
    }

    /// Look up one exact identity. An epoch outside 1..=u32::MAX cannot exist,
    /// so it reads as absent.
    pub async fn get(&self, key: NodePinKey) -> StorageResult<Option<NodePinRecord>> {
        let Ok(epoch) = epoch_to_sql(key.key_epoch) else {
            return Ok(None);
        };
        self.notes
            .with_reader("node_pins_get", move |conn| load(conn, &key, epoch))
            .await
    }
}

#[cfg(test)]
#[path = "node_pins_tests.rs"]
mod tests;
