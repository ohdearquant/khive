//! Transaction-scoped persistence for shared authority records.
//!
//! This is a trusted persistence adapter, not an authorization API. The caller
//! authenticates canonical bodies, checks current authority before replay, and
//! propagates failures. Permission schemas and policy belong to khive-capability.
//! Canonical record bodies are bounded to 2 MiB; request, replay and manifest
//! values have separate 1 MiB limits. Pages contain at most 256 rows plus one probe.
//! These are row/value bounds, not a constant-memory or latency guarantee.

use khive_storage::event::Event;
use khive_storage::types::{SqlStatement, SqlValue, StorageResult};
use khive_storage::{
    AtomicUnitOp, BoxFuture, SqlAccess, SqlWriter, StorageCapability, StorageError,
};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use std::any::Any;
use uuid::Uuid;

mod rows;
use rows::{id, optional_id};
pub use rows::{
    AuthorityChange, AuthorityEpoch, AuthorityPage, AuthorityRecord, AuthorityReplay,
    AuthorityTransition, ChangeKind, RecordKind, ReplayKey, TerminalState,
};

/// Other structural values and terminal-transition bodies are limited to 1 MiB.
pub const MAX_AUTHORITY_VALUE_BYTES: usize = 1024 * 1024;
/// Canonical authenticated record bodies include metadata beyond the request.
pub const MAX_AUTHORITY_RECORD_BYTES: usize = 2 * 1024 * 1024;
/// Complete canonical administration request bytes, independent of record size.
pub const MAX_AUTHORITY_REQUEST_BYTES: usize = 1024 * 1024;
/// Complete serialized replay results, independent of record size.
pub const MAX_AUTHORITY_REPLAY_BYTES: usize = 1024 * 1024;
/// Immutable normalized initialization manifest bytes, independent of record size.
pub const MAX_AUTHORITY_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_AUTHORITY_PAGE_SIZE: u16 = 256;

/// Run one authority unit under the existing SQL transaction owner.
/// The callback must complete on its first poll: only synchronous SQL and bounded
/// local computation are allowed, with no external I/O, blocking waits or real
/// suspension. Results become visible only after commit.
pub async fn transaction<T, F>(sql: &dyn SqlAccess, operation: F) -> StorageResult<T>
where
    T: Any + Send,
    F: for<'w> FnOnce(AuthorityTransaction<'w>) -> BoxFuture<'w, StorageResult<T>> + Send + 'static,
{
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let result = operation(AuthorityTransaction { writer }).await?;
            Ok(Box::new(result) as Box<dyn Any + Send>)
        })
    });
    sql.atomic_unit(op)
        .await?
        .downcast::<T>()
        .map(|result| *result)
        .map_err(|_| StorageError::Internal("authority transaction result type mismatch".into()))
}

/// Borrowed active writer. Its only constructor is the transaction wrapper.
/// It cannot commit, open another connection, or expose raw SQL to its caller.
pub struct AuthorityTransaction<'w> {
    writer: &'w mut dyn SqlWriter,
}

macro_rules! statement {
    ($name:literal, $params:expr) => {
        SqlStatement::new(
            include_str!(concat!("../sql/authority/", $name, ".sql")),
            $params,
        )
        .labelled(concat!("authority.", $name))
    };
}

impl AuthorityTransaction<'_> {
    pub async fn epoch(&mut self) -> StorageResult<Option<AuthorityEpoch>> {
        self.writer
            .query_row(statement!("epoch-get", vec![]))
            .await?
            .map(rows::epoch)
            .transpose()
    }

    pub async fn insert_epoch(&mut self, value: &AuthorityEpoch) -> StorageResult<()> {
        bounded_to("manifest", &value.manifest, MAX_AUTHORITY_MANIFEST_BYTES)?;
        bounded("namespace", value.namespace.as_bytes())?;
        self.writer
            .execute(statement!(
                "epoch-insert",
                vec![
                    id(value.epoch_id),
                    id(value.marker_id),
                    id(value.store_id),
                    SqlValue::Blob(value.manifest_digest.to_vec()),
                    SqlValue::Blob(value.manifest.clone()),
                    SqlValue::Text(value.namespace.clone()),
                    SqlValue::Integer(value.created_at)
                ]
            ))
            .await?;
        self.writer
            .execute(statement!("revision-insert", vec![id(value.epoch_id)]))
            .await?;
        Ok(())
    }

    pub async fn revision(&mut self, epoch_id: Uuid) -> StorageResult<Option<i64>> {
        self.writer
            .query_row(statement!("revision-get", vec![id(epoch_id)]))
            .await?
            .map(|row| {
                let value = rows::integer(&row, "revision")?;
                if value < 0 {
                    return Err(invalid("negative authority revision"));
                }
                Ok(value)
            })
            .transpose()
    }

    /// Compare-and-increment once per actual action; stale/missing/exhausted refuses.
    pub async fn advance_revision(&mut self, epoch_id: Uuid, expected: i64) -> StorageResult<i64> {
        let next = expected
            .checked_add(1)
            .filter(|_| expected >= 0)
            .ok_or_else(|| invalid("authority revision is negative or exhausted"))?;
        let changed = self
            .writer
            .execute(statement!(
                "revision-advance",
                vec![id(epoch_id), SqlValue::Integer(expected)]
            ))
            .await?;
        if changed != 1 {
            return Err(StorageError::Conflict {
                capability: StorageCapability::Sql,
                operation: "authority.advance_revision".into(),
                message: "authority revision did not match".into(),
            });
        }
        Ok(next)
    }

    pub async fn record(
        &mut self,
        epoch_id: Uuid,
        record_id: Uuid,
    ) -> StorageResult<Option<AuthorityRecord>> {
        self.writer
            .query_row(statement!("record-get", vec![id(epoch_id), id(record_id)]))
            .await?
            .map(rows::record)
            .transpose()
    }

    pub async fn insert_record(&mut self, value: &AuthorityRecord) -> StorageResult<()> {
        value.validate()?;
        self.writer
            .execute(statement!(
                "record-insert",
                vec![
                    id(value.id),
                    id(value.epoch_id),
                    SqlValue::Text(value.kind.name().into()),
                    SqlValue::Blob(value.principal.clone()),
                    SqlValue::Text(value.namespace.clone()),
                    optional_id(value.parent_id),
                    SqlValue::from_opt_text(value.bootstrap_slot.as_deref()),
                    optional_id(value.root_anchor_id),
                    SqlValue::Integer(value.created_at),
                    SqlValue::Blob(value.body.clone())
                ]
            ))
            .await?;
        for tag in &value.right_types {
            self.writer
                .execute(statement!(
                    "record-right-type-insert",
                    vec![
                        id(value.epoch_id),
                        id(value.id),
                        SqlValue::Text(tag.clone())
                    ]
                ))
                .await?;
        }
        Ok(())
    }

    /// Bounded candidate discovery; this method establishes no read or grant authority.
    pub async fn records_for_principal(
        &mut self,
        epoch_id: Uuid,
        principal: &[u8],
        after: Option<Uuid>,
        limit: u16,
    ) -> StorageResult<AuthorityPage<AuthorityRecord>> {
        bounded("principal", principal)?;
        let probe = page_probe(limit)?;
        let rows = self
            .writer
            .query_all(statement!(
                "records-principal",
                vec![
                    id(epoch_id),
                    SqlValue::Blob(principal.to_vec()),
                    cursor(after),
                    SqlValue::Integer(probe)
                ]
            ))
            .await?;
        let values = rows
            .into_iter()
            .map(rows::record)
            .collect::<StorageResult<Vec<_>>>()?;
        Ok(page(values, limit, |value| value.id))
    }

    pub async fn transition(
        &mut self,
        epoch_id: Uuid,
        record_id: Uuid,
    ) -> StorageResult<Option<AuthorityTransition>> {
        self.writer
            .query_row(statement!(
                "transition-get",
                vec![id(epoch_id), id(record_id)]
            ))
            .await?
            .map(rows::transition)
            .transpose()
    }

    pub async fn insert_transition(&mut self, value: &AuthorityTransition) -> StorageResult<()> {
        bounded("transition body", &value.body)?;
        self.writer
            .execute(statement!(
                "transition-insert",
                vec![
                    id(value.id),
                    id(value.epoch_id),
                    id(value.record_id),
                    SqlValue::Text(value.state.name().into()),
                    id(value.action_id),
                    id(value.event_id),
                    SqlValue::Integer(value.created_at),
                    SqlValue::Blob(value.body.clone())
                ]
            ))
            .await?;
        Ok(())
    }

    /// Append the canonical core event and its indexed action reference together.
    pub async fn append_change(
        &mut self,
        change: &AuthorityChange,
        event: &Event,
    ) -> StorageResult<()> {
        if event.id != change.event_id
            || event.kind != EventKind::GrantChange
            || event.substrate != SubstrateKind::Event
            || event.outcome != EventOutcome::Success
            || event.payload_schema_version != 1
        {
            return Err(invalid(
                "authority change requires its matching successful GrantChange event",
            ));
        }
        crate::stores::event::append_event_on_writer(self.writer, event).await?;
        self.writer
            .execute(statement!(
                "change-insert",
                vec![
                    id(change.event_id),
                    id(change.epoch_id),
                    id(change.action_id),
                    SqlValue::Text(change.kind.name().into()),
                    id(change.record_id)
                ]
            ))
            .await?;
        Ok(())
    }

    /// Read indexed event identities; the service applies visibility before public paging.
    pub async fn changes_for_action(
        &mut self,
        epoch_id: Uuid,
        action_id: Uuid,
        after: Option<Uuid>,
        limit: u16,
    ) -> StorageResult<AuthorityPage<AuthorityChange>> {
        let probe = page_probe(limit)?;
        let rows = self
            .writer
            .query_all(statement!(
                "changes-action",
                vec![
                    id(epoch_id),
                    id(action_id),
                    cursor(after),
                    SqlValue::Integer(probe)
                ]
            ))
            .await?;
        let values = rows
            .into_iter()
            .map(rows::change)
            .collect::<StorageResult<Vec<_>>>()?;
        Ok(page(values, limit, |value| value.event_id))
    }

    /// The service must recheck current authorization before calling this method.
    pub async fn replay(&mut self, key: &ReplayKey) -> StorageResult<Option<AuthorityReplay>> {
        self.writer
            .query_row(statement!("replay-get", key.params()?))
            .await?
            .map(|row| {
                Ok(AuthorityReplay {
                    canonical_request: rows::blob_with_limit(
                        &row,
                        "canonical_request",
                        MAX_AUTHORITY_REQUEST_BYTES,
                    )?,
                    result: rows::blob_with_limit(&row, "result", MAX_AUTHORITY_REPLAY_BYTES)?,
                })
            })
            .transpose()
    }

    pub async fn insert_replay(
        &mut self,
        key: &ReplayKey,
        value: &AuthorityReplay,
    ) -> StorageResult<()> {
        bounded_to(
            "canonical request",
            &value.canonical_request,
            MAX_AUTHORITY_REQUEST_BYTES,
        )?;
        bounded_to("replay result", &value.result, MAX_AUTHORITY_REPLAY_BYTES)?;
        let mut params = key.params()?;
        params.push(SqlValue::Blob(value.canonical_request.clone()));
        params.push(SqlValue::Blob(value.result.clone()));
        self.writer
            .execute(statement!("replay-insert", params))
            .await?;
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: "authority persistence".into(),
        message: message.into(),
    }
}
fn bounded(name: &str, bytes: &[u8]) -> StorageResult<()> {
    bounded_to(name, bytes, MAX_AUTHORITY_VALUE_BYTES)
}
fn bounded_to(name: &str, bytes: &[u8], maximum: usize) -> StorageResult<()> {
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(invalid(format!(
            "{name} has {} bytes; supported range is 1..={maximum}",
            bytes.len()
        )));
    }
    Ok(())
}
fn page_probe(limit: u16) -> StorageResult<i64> {
    if limit == 0 || limit > MAX_AUTHORITY_PAGE_SIZE {
        return Err(invalid(format!(
            "page size {limit}; supported range is 1..={MAX_AUTHORITY_PAGE_SIZE}"
        )));
    }
    Ok(i64::from(limit) + 1)
}
fn cursor(after: Option<Uuid>) -> SqlValue {
    SqlValue::Text(after.map(|id| id.to_string()).unwrap_or_default())
}
fn page<T>(mut items: Vec<T>, limit: u16, id_of: impl Fn(&T) -> Uuid) -> AuthorityPage<T> {
    let more = items.len() > usize::from(limit);
    items.truncate(usize::from(limit));
    let next_id = if more { items.last().map(id_of) } else { None };
    AuthorityPage { items, next_id }
}

#[cfg(test)]
mod tests;
