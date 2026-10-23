//! Durable obligations for deferred entity indexing (ADR-014 Amendment 7).
//!
//! Deferred publication uses the writer passed to `SqlAccess::atomic_unit`;
//! namespace moves use their caller's existing native transaction. No helper
//! opens a transaction, embeds, retries, or settles an unknown outcome.

use std::collections::{BTreeMap, BTreeSet};

use khive_storage::{
    SqlReader, SqlRow, SqlStatement, SqlValue, SqlWriter, StorageError, StorageResult,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const READ: &str = include_str!("../sql/pending-entity-index-read.sql");
const INSERT: &str = include_str!("../sql/pending-entity-index-insert.sql");
const REPLACE: &str = include_str!("../sql/pending-entity-index-replace.sql");
const REMOVE: &str = include_str!("../sql/pending-entity-index-remove.sql");
const LIVE: &str = include_str!("../sql/pending-entity-index-live.sql");
const MOVE_PAGE: &str = include_str!("../sql/pending-entity-index-move-page.sql");
const MOVE_PAGE_SIZE: i64 = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingIndexTargetKind {
    Fts,
    Vector {
        model: String,
        dimensions: Vec<usize>,
    },
}

impl PendingIndexTargetKind {
    /// Conflicting definitions retain one target and neither dimension wins.
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Vector { dimensions, .. } if dimensions.len() != 1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingIndexTarget {
    pub kind: PendingIndexTargetKind,
    pub done: bool,
    pub attempts: u64,
    pub last_error: Option<String>,
    /// UTC microseconds. The caller owns retry scheduling policy.
    pub next_eligible_at: i64,
}

impl PendingIndexTarget {
    fn new(kind: PendingIndexTargetKind, now: i64) -> Self {
        Self {
            kind,
            done: false,
            attempts: 0,
            last_error: None,
            next_eligible_at: now,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEntityIndex {
    pub entity_id: Uuid,
    pub namespace: String,
    pub source_version: i64,
    pub generation: Uuid,
    /// UTC microseconds, preserved while any obligation remains pending.
    pub first_pending_at: i64,
    pub targets: Vec<PendingIndexTarget>,
}

fn invalid(message: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(format!("pending entity index: {message}"))
}

fn valid_category(category: &str) -> bool {
    !category.is_empty()
        && category.len() <= 64
        && category
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate(record: &PendingEntityIndex) -> StorageResult<()> {
    if record.source_version <= 0 || record.generation.is_nil() {
        return Err(invalid("invalid source version or generation"));
    }
    let mut fts = 0;
    let mut models = BTreeSet::new();
    for target in &record.targets {
        if target
            .last_error
            .as_deref()
            .is_some_and(|category| !valid_category(category))
        {
            return Err(invalid("invalid error category"));
        }
        match &target.kind {
            PendingIndexTargetKind::Fts => fts += 1,
            PendingIndexTargetKind::Vector { model, dimensions } => {
                if model.is_empty()
                    || !models.insert(model)
                    || dimensions.is_empty()
                    || dimensions.contains(&0)
                    || dimensions.windows(2).any(|pair| pair[0] >= pair[1])
                    || (target.done && target.kind.is_blocked())
                {
                    return Err(invalid("invalid vector target definition"));
                }
            }
        }
    }
    if fts != 1 {
        return Err(invalid("exactly one FTS target is required"));
    }
    Ok(())
}

fn decode_fields(
    entity_id: &str,
    namespace: &str,
    source_version: i64,
    generation: &str,
    first_pending_at: i64,
    targets_json: &str,
) -> StorageResult<PendingEntityIndex> {
    let record = PendingEntityIndex {
        entity_id: Uuid::parse_str(entity_id).map_err(invalid)?,
        namespace: namespace.to_owned(),
        source_version,
        generation: Uuid::parse_str(generation).map_err(invalid)?,
        first_pending_at,
        targets: serde_json::from_str(targets_json).map_err(invalid)?,
    };
    validate(&record)?;
    Ok(record)
}

fn decode(row: SqlRow) -> StorageResult<PendingEntityIndex> {
    decode_fields(
        row.text("entity_id").map_err(invalid)?,
        row.text("namespace").map_err(invalid)?,
        row.i64("source_version").map_err(invalid)?,
        row.text("generation").map_err(invalid)?,
        row.i64("first_pending_at").map_err(invalid)?,
        row.text("targets_json").map_err(invalid)?,
    )
}

fn values(record: &PendingEntityIndex) -> StorageResult<Vec<SqlValue>> {
    validate(record)?;
    Ok(vec![
        SqlValue::Text(record.entity_id.to_string()),
        SqlValue::Text(record.namespace.clone()),
        SqlValue::Integer(record.source_version),
        SqlValue::Text(record.generation.to_string()),
        SqlValue::Integer(record.first_pending_at),
        SqlValue::Text(serde_json::to_string(&record.targets).map_err(invalid)?),
    ])
}

/// Read by entity id in the resolved domain store, never by a namespace key.
pub async fn read<R: SqlReader + ?Sized>(
    reader: &mut R,
    entity_id: Uuid,
) -> StorageResult<Option<PendingEntityIndex>> {
    reader
        .query_row(
            SqlStatement::new(READ, vec![SqlValue::Text(entity_id.to_string())])
                .labelled("pending-entity-index-read"),
        )
        .await?
        .map(decode)
        .transpose()
}

/// Check the source revision in the publication transaction before index writes.
pub async fn live_matches<R: SqlReader + ?Sized>(
    reader: &mut R,
    record: &PendingEntityIndex,
) -> StorageResult<bool> {
    Ok(reader
        .query_row(
            SqlStatement::new(
                LIVE,
                vec![
                    SqlValue::Text(record.entity_id.to_string()),
                    SqlValue::Text(record.namespace.clone()),
                    SqlValue::Integer(record.source_version),
                ],
            )
            .labelled("pending-entity-index-live"),
        )
        .await?
        .is_some())
}

fn coalesce(
    old: Option<&PendingEntityIndex>,
    models: &[(String, usize)],
    now: i64,
) -> StorageResult<Vec<PendingIndexTarget>> {
    let mut fts = old
        .and_then(|record| {
            record
                .targets
                .iter()
                .find(|target| target.kind == PendingIndexTargetKind::Fts)
        })
        .cloned()
        .unwrap_or_else(|| PendingIndexTarget::new(PendingIndexTargetKind::Fts, now));
    fts.done = false;
    let mut vectors = BTreeMap::<String, PendingIndexTarget>::new();
    if let Some(old) = old {
        for target in &old.targets {
            if let PendingIndexTargetKind::Vector { model, .. } = &target.kind {
                if !target.done {
                    vectors.insert(model.clone(), target.clone());
                }
            }
        }
    }
    let mut captured = BTreeMap::<String, BTreeSet<usize>>::new();
    for (model, dimensions) in models {
        if model.is_empty() || *dimensions == 0 {
            return Err(invalid("invalid captured model definition"));
        }
        captured
            .entry(model.clone())
            .or_default()
            .insert(*dimensions);
    }
    for (model, dimensions) in captured {
        let target = vectors.entry(model.clone()).or_insert_with(|| {
            old.and_then(|record| record.targets.iter().find(|target| {
                matches!(&target.kind, PendingIndexTargetKind::Vector { model: old_model, dimensions: old_dimensions }
                    if old_model == &model && old_dimensions.iter().any(|dimension| dimensions.contains(dimension)))
            })).cloned().unwrap_or_else(|| PendingIndexTarget::new(
                PendingIndexTargetKind::Vector { model: model.clone(), dimensions: dimensions.iter().copied().collect() }, now))
        });
        if let PendingIndexTargetKind::Vector {
            dimensions: definitions,
            ..
        } = &mut target.kind
        {
            definitions.extend(dimensions);
            definitions.sort_unstable();
            definitions.dedup();
        }
    }
    let mut targets = vec![fts];
    targets.extend(vectors.into_values().map(|mut target| {
        target.done = false;
        target
    }));
    Ok(targets)
}

/// Enqueue after the entity replacement in the same atomic writer unit.
/// `models` is the pre-transaction registry snapshot; `generation` must be newly
/// minted for this replacement. A failure must roll back the entity mutation.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue(
    writer: &mut dyn SqlWriter,
    entity_id: Uuid,
    namespace: &str,
    source_version: i64,
    models: &[(String, usize)],
    generation: Uuid,
    now: i64,
) -> StorageResult<PendingEntityIndex> {
    let old = read(writer, entity_id).await?;
    if old.as_ref().is_some_and(|old| old.generation == generation) {
        return Err(invalid("replacement requires a new generation"));
    }
    let record = PendingEntityIndex {
        entity_id,
        namespace: namespace.to_owned(),
        source_version,
        generation,
        first_pending_at: old.as_ref().map_or(now, |old| old.first_pending_at),
        targets: coalesce(old.as_ref(), models, now)?,
    };
    if !live_matches(writer, &record).await? {
        return Err(invalid("enqueue source revision is not live"));
    }
    let changed = if let Some(old) = old {
        replace(writer, old.generation, &record).await?
    } else {
        writer
            .execute(
                SqlStatement::new(INSERT, values(&record)?).labelled("pending-entity-index-insert"),
            )
            .await?
            == 1
    };
    if !changed {
        return Err(invalid("enqueue generation changed"));
    }
    Ok(record)
}

/// Save state only for the generation observed within this atomic writer unit.
pub async fn replace(
    writer: &mut dyn SqlWriter,
    observed_generation: Uuid,
    record: &PendingEntityIndex,
) -> StorageResult<bool> {
    let mut params = values(record)?;
    params.push(SqlValue::Text(observed_generation.to_string()));
    Ok(writer
        .execute(SqlStatement::new(REPLACE, params).labelled("pending-entity-index-replace"))
        .await?
        == 1)
}

/// Removal does not itself prove publication; the caller owns source/index guards.
pub async fn remove(
    writer: &mut dyn SqlWriter,
    entity_id: Uuid,
    observed_generation: Uuid,
) -> StorageResult<bool> {
    Ok(writer
        .execute(
            SqlStatement::new(
                REMOVE,
                vec![
                    SqlValue::Text(entity_id.to_string()),
                    SqlValue::Text(observed_generation.to_string()),
                ],
            )
            .labelled("pending-entity-index-remove"),
        )
        .await?
        == 1)
}

/// Consume only after publishing this target in the same atomic writer unit.
/// A false result requires that unit to roll back any preceding index writes.
pub async fn finish(
    writer: &mut dyn SqlWriter,
    entity_id: Uuid,
    observed_generation: Uuid,
    kind: &PendingIndexTargetKind,
) -> StorageResult<bool> {
    let Some(mut record) = read(writer, entity_id).await? else {
        return Ok(false);
    };
    if record.generation != observed_generation || kind.is_blocked() {
        return Ok(false);
    }
    let Some(target) = record
        .targets
        .iter_mut()
        .find(|target| target.kind == *kind && !target.done)
    else {
        return Ok(false);
    };
    target.done = true;
    if record.targets.iter().all(|target| target.done) {
        remove(writer, entity_id, observed_generation).await
    } else {
        replace(writer, observed_generation, &record).await
    }
}

/// Record a settled failure only; unknown transaction outcomes must not call this.
/// Categories must be sanitized identifiers, never an error message or entity text.
pub async fn record_failure(
    writer: &mut dyn SqlWriter,
    entity_id: Uuid,
    observed_generation: Uuid,
    kind: &PendingIndexTargetKind,
    category: &str,
    next_eligible_at: i64,
) -> StorageResult<bool> {
    if !valid_category(category) {
        return Err(invalid(
            "failure category must be a short lowercase identifier",
        ));
    }
    let Some(mut record) = read(writer, entity_id).await? else {
        return Ok(false);
    };
    if record.generation != observed_generation {
        return Ok(false);
    }
    let Some(target) = record
        .targets
        .iter_mut()
        .find(|target| target.kind == *kind && !target.done)
    else {
        return Ok(false);
    };
    target.attempts = target.attempts.saturating_add(1);
    target.last_error = Some(category.to_owned());
    target.next_eligible_at = target.next_eligible_at.max(next_eligible_at);
    replace(writer, observed_generation, &record).await
}

fn move_generation(
    old: Uuid,
    entity_id: Uuid,
    namespace: &str,
    source_version: i64,
) -> StorageResult<Uuid> {
    if old.is_nil() || source_version <= 0 {
        return Err(invalid("invalid namespace move generation input"));
    }
    let mut name = b"khive.pending-entity-index.namespace-move.v1\0".to_vec();
    name.extend_from_slice(entity_id.as_bytes());
    let length = u64::try_from(namespace.len()).map_err(invalid)?;
    name.extend_from_slice(&length.to_be_bytes());
    name.extend_from_slice(namespace.as_bytes());
    name.extend_from_slice(&source_version.to_be_bytes());
    let generation = Uuid::new_v5(&old, &name);
    if generation.is_nil() || generation == old {
        return Err(invalid("namespace move generation was reused"));
    }
    Ok(generation)
}

fn refresh_for_live_revision(
    old: &PendingEntityIndex,
    namespace: &str,
    source_version: i64,
    generation: Uuid,
) -> StorageResult<PendingEntityIndex> {
    validate(old)?;
    if generation.is_nil() || generation == old.generation {
        return Err(invalid("namespace move generation was reused"));
    }
    let mut record = old.clone();
    record.namespace = namespace.to_owned();
    record.source_version = source_version;
    record.generation = generation;
    for target in &mut record.targets {
        target.done = false;
    }
    validate(&record)?;
    Ok(record)
}

fn native_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

fn decode_move_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(PendingEntityIndex, i64)> {
    let record = decode_fields(
        &row.get::<_, String>("entity_id")?,
        &row.get::<_, String>("namespace")?,
        row.get("source_version")?,
        &row.get::<_, String>("generation")?,
        row.get("first_pending_at")?,
        &row.get::<_, String>("targets_json")?,
    )
    .map_err(native_error)?;
    Ok((record, row.get("live_version")?))
}

/// Refresh obligations before the matching entity route, within the caller's move transaction.
pub(crate) fn refresh_for_namespace_move(
    conn: &rusqlite::Connection,
    source: &str,
    target: &str,
    kind: &str,
) -> rusqlite::Result<u64> {
    let mut after: Option<String> = None;
    let mut refreshed = 0_u64;
    loop {
        let page = {
            let mut statement = conn.prepare(MOVE_PAGE)?;
            let rows = statement.query_map(
                rusqlite::params![source, kind, after, MOVE_PAGE_SIZE],
                decode_move_row,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if page.is_empty() {
            break;
        }
        for (old, live_version) in page {
            after = Some(old.entity_id.to_string());
            let version = live_version
                .checked_add(1)
                .ok_or_else(|| native_error(invalid("entity version overflow during move")))?;
            let generation = move_generation(old.generation, old.entity_id, target, version)
                .map_err(native_error)?;
            let record = refresh_for_live_revision(&old, target, version, generation)
                .map_err(native_error)?;
            let changed = conn.execute(
                REPLACE,
                rusqlite::params![
                    record.entity_id.to_string(),
                    record.namespace,
                    record.source_version,
                    record.generation.to_string(),
                    record.first_pending_at,
                    serde_json::to_string(&record.targets).map_err(native_error)?,
                    old.generation.to_string(),
                ],
            )?;
            if changed != 1 {
                return Err(native_error(invalid("namespace move generation changed")));
            }
            refreshed = refreshed
                .checked_add(1)
                .ok_or_else(|| native_error(invalid("namespace move row count overflow")))?;
        }
    }
    Ok(refreshed)
}

#[cfg(test)]
mod tests;
