//! Atomic mixed entity/note creation for `create(items=[...], atomic=true)`.
//!
//! Validation completes before the writer is acquired. Entity/note rows and
//! their FTS documents then commit in one transaction. Vector indexing for
//! newly-created notes runs after commit and is reported separately.

use std::any::Any;
use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use khive_db::stores::entity::entity_upsert_statement;
use khive_db::stores::note::{note_assign_seq_statement, note_insert_or_ignore_statement};
use khive_db::stores::text::{delete_document_statement, insert_document_statement};
use khive_storage::note::Note;
use khive_storage::{
    AtomicUnitOp, Entity, SqlRow, SqlStatement, SqlValue, StorageError, SubstrateKind,
};

use crate::curation::{
    entity_fts_document, note_embedding_text_ref, note_fts_document, NoteReindexFailure,
    NoteReindexOutcome,
};
use crate::operations::EntityCreateSpec;
pub use crate::operations::NotePostCommitFailureStage as BulkPostCommitFailureStage;
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

/// Fully-specified note input for an atomic bulk create.
#[derive(Clone, Debug)]
pub struct BulkNoteCreateSpec {
    pub kind: String,
    pub name: Option<String>,
    pub content: String,
    pub salience: Option<f64>,
    pub properties: Option<Value>,
    /// Canonical natural key already copied into `properties.external_id`.
    pub external_id: Option<String>,
}

/// One record in an atomic mixed create batch.
#[derive(Clone, Debug)]
pub enum BulkRecordCreateSpec {
    Entity(EntityCreateSpec),
    Note(BulkNoteCreateSpec),
}

/// Persisted record returned in submitted order.
#[derive(Clone, Debug)]
pub enum BulkCreatedRecord {
    Entity(Entity),
    Note(Note),
}

impl BulkCreatedRecord {
    #[must_use]
    pub fn id(&self) -> Uuid {
        match self {
            Self::Entity(entity) => entity.id,
            Self::Note(note) => note.id,
        }
    }
}

/// Per-input result. `created=false` identifies a natural-key note retry and
/// `record` is the canonical existing row.
#[derive(Clone, Debug)]
pub struct BulkRecordCreateOutcome {
    pub record: BulkCreatedRecord,
    pub created: bool,
}

/// Structured repair diagnostic for a note whose row committed successfully.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BulkPostCommitFailure {
    #[serde(rename = "id")]
    pub note_id: Uuid,
    pub stages: Vec<BulkPostCommitFailureStage>,
}

/// Atomic mixed-create result plus post-commit indexing diagnostics.
#[derive(Clone, Debug, Default)]
pub struct BulkRecordCreateResult {
    pub outcomes: Vec<BulkRecordCreateOutcome>,
    pub embedding_truncation: crate::retrieval::EmbeddingTruncationReport,
    pub post_commit_failures: Vec<BulkPostCommitFailure>,
}

#[derive(Clone, Debug)]
enum PreparedRecord {
    Entity(Entity),
    Note {
        note: Note,
        external_id: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct StoredOutcome {
    record: BulkCreatedRecord,
    created: bool,
}

#[cfg(test)]
static DELETE_NOTE_AFTER_ATOMIC_COMMIT: std::sync::Mutex<Option<(String, Uuid)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
struct PostCommitPause {
    committed: tokio::sync::oneshot::Sender<Uuid>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
static POST_COMMIT_PAUSES: std::sync::LazyLock<std::sync::Mutex<HashMap<String, PostCommitPause>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn properties_external_id(properties: Option<&Value>) -> RuntimeResult<Option<String>> {
    match properties.and_then(|value| value.get("external_id")) {
        None => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(RuntimeError::InvalidInput(
            "external_id must be a non-empty string".into(),
        )),
    }
}

fn sql_value_kind(value: &SqlValue) -> &'static str {
    match value {
        SqlValue::Null => "null",
        SqlValue::Bool(_) => "bool",
        SqlValue::Integer(_) => "integer",
        SqlValue::Float(_) => "float",
        SqlValue::Text(_) => "text",
        SqlValue::Blob(_) => "blob",
        SqlValue::Json(_) => "json",
        SqlValue::Uuid(_) => "uuid",
        SqlValue::Timestamp(_) => "timestamp",
    }
}

fn note_column_error(column: &str, expected: &str, actual: Option<&SqlValue>) -> StorageError {
    let actual = actual.map_or("missing", sql_value_kind);
    StorageError::Internal(format!(
        "transaction note row column '{column}' expected {expected}, got {actual}"
    ))
}

fn required_note_text(row: &SqlRow, column: &str) -> Result<String, StorageError> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        actual => Err(note_column_error(column, "text", actual)),
    }
}

fn optional_note_text(row: &SqlRow, column: &str) -> Result<Option<String>, StorageError> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        actual => Err(note_column_error(column, "text or null", actual)),
    }
}

fn optional_note_float(row: &SqlRow, column: &str) -> Result<Option<f64>, StorageError> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Float(value)) => Ok(Some(*value)),
        Some(SqlValue::Integer(value)) => Ok(Some(*value as f64)),
        actual => Err(note_column_error(column, "number or null", actual)),
    }
}

fn required_note_integer(row: &SqlRow, column: &str) -> Result<i64, StorageError> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        Some(SqlValue::Timestamp(value)) => Ok(value.timestamp_micros()),
        actual => Err(note_column_error(column, "integer", actual)),
    }
}

fn optional_note_integer(row: &SqlRow, column: &str) -> Result<Option<i64>, StorageError> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => Ok(Some(*value)),
        Some(SqlValue::Timestamp(value)) => Ok(Some(value.timestamp_micros())),
        actual => Err(note_column_error(column, "integer or null", actual)),
    }
}

fn note_properties(row: &SqlRow) -> Result<Option<Value>, StorageError> {
    match row.get("properties") {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Json(value)) => Ok(Some(value.clone())),
        Some(SqlValue::Text(value)) => serde_json::from_str(value).map(Some).map_err(|error| {
            StorageError::Internal(format!(
                "transaction note row properties contained invalid JSON: {error}"
            ))
        }),
        actual => Err(note_column_error(
            "properties",
            "json, text, or null",
            actual,
        )),
    }
}

fn note_from_transaction_row(row: Option<SqlRow>) -> Result<Note, StorageError> {
    let Some(row) = row else {
        return Err(StorageError::Internal(
            "note insert completed but its transaction snapshot row was not found".into(),
        ));
    };
    let id = match row.get("id") {
        Some(SqlValue::Uuid(value)) => *value,
        Some(SqlValue::Text(value)) => Uuid::parse_str(value).map_err(|error| {
            StorageError::Internal(format!(
                "transaction note row returned invalid UUID: {error}"
            ))
        })?,
        actual => return Err(note_column_error("id", "uuid or text", actual)),
    };

    Ok(Note {
        id,
        namespace: required_note_text(&row, "namespace")?,
        kind: required_note_text(&row, "kind")?,
        status: required_note_text(&row, "status")?,
        name: optional_note_text(&row, "name")?,
        content: required_note_text(&row, "content")?,
        salience: optional_note_float(&row, "salience")?,
        decay_factor: optional_note_float(&row, "decay_factor")?,
        expires_at: optional_note_integer(&row, "expires_at")?,
        properties: note_properties(&row)?,
        created_at: required_note_integer(&row, "created_at")?,
        updated_at: required_note_integer(&row, "updated_at")?,
        deleted_at: optional_note_integer(&row, "deleted_at")?,
    })
}

fn note_transaction_row_by_id(id: Uuid) -> SqlStatement {
    SqlStatement {
        sql: "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
                     expires_at, properties, created_at, updated_at, deleted_at \
                FROM notes WHERE id = ?1"
            .to_string(),
        params: vec![SqlValue::Uuid(id)],
        label: Some("bulk-note-transaction-row".to_string()),
    }
}

fn canonical_note_transaction_row(note: &Note, external_id: String) -> SqlStatement {
    SqlStatement {
        sql: "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
                     expires_at, properties, created_at, updated_at, deleted_at \
                FROM notes \
               WHERE namespace = ?1 AND kind = ?2 \
                 AND json_extract(properties, '$.external_id') = ?3 \
                 AND deleted_at IS NULL \
               ORDER BY rowid ASC LIMIT 1"
            .to_string(),
        params: vec![
            SqlValue::Text(note.namespace.clone()),
            SqlValue::Text(note.kind.clone()),
            SqlValue::Text(external_id),
        ],
        label: Some("bulk-note-canonical-transaction-row".to_string()),
    }
}

#[cfg(test)]
fn arm_delete_note_after_atomic_commit(namespace: &str, id: Uuid) {
    let mut armed = DELETE_NOTE_AFTER_ATOMIC_COMMIT
        .lock()
        .expect("atomic-create test seam mutex poisoned");
    assert!(armed.is_none(), "atomic-create test seam already armed");
    *armed = Some((namespace.to_string(), id));
}

#[cfg(test)]
fn take_delete_note_after_atomic_commit(namespace: &str) -> Option<Uuid> {
    let mut armed = DELETE_NOTE_AFTER_ATOMIC_COMMIT
        .lock()
        .expect("atomic-create test seam mutex poisoned");
    if matches!(armed.as_ref(), Some((armed_namespace, _)) if armed_namespace == namespace) {
        armed.take().map(|(_, id)| id)
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) fn arm_post_commit_pause(
    namespace: &str,
) -> (
    tokio::sync::oneshot::Receiver<Uuid>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let previous = POST_COMMIT_PAUSES
        .lock()
        .expect("atomic-create pause mutex poisoned")
        .insert(
            namespace.to_string(),
            PostCommitPause {
                committed: committed_tx,
                resume: resume_rx,
            },
        );
    assert!(previous.is_none(), "atomic-create pause already armed");
    (committed_rx, resume_tx)
}

#[cfg(test)]
async fn pause_before_note_post_commit_index(committed: &Note) {
    let pause = POST_COMMIT_PAUSES
        .lock()
        .expect("atomic-create pause mutex poisoned")
        .remove(&committed.namespace);
    let Some(pause) = pause else {
        return;
    };
    pause
        .committed
        .send(committed.id)
        .expect("atomic-create pause observer dropped");
    pause
        .resume
        .await
        .expect("atomic-create pause controller dropped");
}

async fn guarded_upsert_note_fts(runtime: &KhiveRuntime, committed: &Note) -> RuntimeResult<bool> {
    let namespace = committed.namespace.clone();
    let witness = note_revision_witness_statement(committed, "note-fts-current-revision");
    let delete = delete_document_statement("fts_notes", &namespace, committed.id);
    let insert = insert_document_statement("fts_notes", &note_fts_document(committed));

    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let current = writer.query_row(witness).await?.is_some();
            if !current {
                return Ok(Box::new(false) as Box<dyn Any + Send>);
            }

            writer.execute(delete).await?;
            writer.execute(insert).await?;
            Ok(Box::new(true) as Box<dyn Any + Send>)
        })
    });

    let outcome = runtime.sql().atomic_unit(op).await?;
    outcome
        .downcast::<bool>()
        .map(|value| *value)
        .map_err(|_| RuntimeError::Internal("guarded note FTS outcome had unexpected type".into()))
}

fn f32_vec_to_bytes(data: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for value in data {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    bytes
}

fn note_revision_witness_statement(committed: &Note, label: &str) -> SqlStatement {
    let properties = committed
        .properties
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_default());
    SqlStatement {
        sql: "SELECT 1 AS current FROM notes \
              WHERE id = ?1 AND namespace = ?2 AND kind = ?3 AND status = ?4 \
                AND name IS ?5 AND content = ?6 AND salience IS ?7 \
                AND decay_factor IS ?8 AND expires_at IS ?9 AND properties IS ?10 \
                AND updated_at = ?11 AND deleted_at IS NULL"
            .to_string(),
        params: vec![
            SqlValue::Uuid(committed.id),
            SqlValue::Text(committed.namespace.clone()),
            SqlValue::Text(committed.kind.clone()),
            SqlValue::Text(committed.status.clone()),
            committed
                .name
                .clone()
                .map_or(SqlValue::Null, SqlValue::Text),
            SqlValue::Text(committed.content.clone()),
            committed.salience.map_or(SqlValue::Null, SqlValue::Float),
            committed
                .decay_factor
                .map_or(SqlValue::Null, SqlValue::Float),
            committed
                .expires_at
                .map_or(SqlValue::Null, SqlValue::Integer),
            properties.map_or(SqlValue::Null, SqlValue::Text),
            SqlValue::Integer(committed.updated_at),
        ],
        label: Some(label.to_string()),
    }
}

fn note_matches_committed_witness(current: &Note, committed: &Note) -> bool {
    current.deleted_at.is_none()
        && current.id == committed.id
        && current.namespace == committed.namespace
        && current.kind == committed.kind
        && current.status == committed.status
        && current.name == committed.name
        && current.updated_at == committed.updated_at
        && current.content == committed.content
        && current.salience == committed.salience
        && current.decay_factor == committed.decay_factor
        && current.expires_at == committed.expires_at
        && current.properties == committed.properties
}

async fn guarded_insert_note_vectors(
    runtime: &KhiveRuntime,
    committed: &Note,
    embeddings: Vec<(String, Vec<f32>)>,
) -> RuntimeResult<bool> {
    let writes: Vec<_> = embeddings
        .into_iter()
        .map(|(model, embedding)| {
            (
                format!("vec_{}", crate::config::sanitize_key(&model)),
                model,
                f32_vec_to_bytes(&embedding),
            )
        })
        .collect();
    let subject = committed.id.to_string();
    let namespace = committed.namespace.clone();
    let kind = SubstrateKind::Note.to_string();
    let field = "note.content".to_string();
    let witness = note_revision_witness_statement(committed, "bulk-note-vector-current-revision");

    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let current = writer.query_row(witness).await?.is_some();
            if !current {
                return Ok(Box::new(false) as Box<dyn Any + Send>);
            }

            for (table, model, blob) in writes {
                writer
                    .execute(SqlStatement {
                        sql: format!(
                            "INSERT INTO ann_write_log \
                             (namespace, embedding_model, kind, field, subject_id, op) \
                             SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                             FROM {table} WHERE subject_id = ?1 AND NOT \
                             (namespace = ?2 AND embedding_model = ?3 AND kind = ?4 AND field = ?5)"
                        ),
                        params: vec![
                            SqlValue::Text(subject.clone()),
                            SqlValue::Text(namespace.clone()),
                            SqlValue::Text(model.clone()),
                            SqlValue::Text(kind.clone()),
                            SqlValue::Text(field.clone()),
                        ],
                        label: Some("bulk-note-vector-log-delete".to_string()),
                    })
                    .await?;
                writer
                    .execute(SqlStatement {
                        sql: format!("DELETE FROM {table} WHERE subject_id = ?1"),
                        params: vec![SqlValue::Text(subject.clone())],
                        label: Some("bulk-note-vector-delete".to_string()),
                    })
                    .await?;
                writer
                    .execute(SqlStatement {
                        sql: format!(
                            "INSERT INTO {table} \
                             (subject_id, namespace, kind, field, embedding_model, embedding) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                        ),
                        params: vec![
                            SqlValue::Text(subject.clone()),
                            SqlValue::Text(namespace.clone()),
                            SqlValue::Text(kind.clone()),
                            SqlValue::Text(field.clone()),
                            SqlValue::Text(model.clone()),
                            SqlValue::Blob(blob),
                        ],
                        label: Some("bulk-note-vector-insert".to_string()),
                    })
                    .await?;
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO ann_write_log \
                              (namespace, embedding_model, kind, field, subject_id, op) \
                              VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')"
                            .to_string(),
                        params: vec![
                            SqlValue::Text(namespace.clone()),
                            SqlValue::Text(model),
                            SqlValue::Text(kind.clone()),
                            SqlValue::Text(field.clone()),
                            SqlValue::Text(subject.clone()),
                        ],
                        label: Some("bulk-note-vector-log-upsert".to_string()),
                    })
                    .await?;
            }
            Ok(Box::new(true) as Box<dyn Any + Send>)
        })
    });

    let outcome = runtime.sql().atomic_unit(op).await?;
    outcome.downcast::<bool>().map(|value| *value).map_err(|_| {
        RuntimeError::Internal("guarded bulk-note vector outcome had unexpected type".into())
    })
}

enum NoteRevisionIndexAttempt {
    Complete(NoteReindexOutcome),
    Stale(NoteReindexOutcome),
}

async fn index_note_revision_once(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    committed: &Note,
    embedding_text: &str,
    model_names: &[String],
    include_fts: bool,
) -> NoteRevisionIndexAttempt {
    let mut outcome = NoteReindexOutcome::default();

    if include_fts {
        let fts_result = {
            #[cfg(any(test, feature = "fault-injection"))]
            let injected = crate::operations::consume_fts_fail_fault(&committed.namespace);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let injected = false;

            if injected {
                Err(RuntimeError::Internal("injected FTS failure".to_string()))
            } else if let Err(error) = runtime.text_for_notes(token) {
                Err(error)
            } else {
                guarded_upsert_note_fts(runtime, committed).await
            }
        };
        match fts_result {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    note_id = %committed.id,
                    "note changed or was deleted before post-commit FTS work"
                );
                return NoteRevisionIndexAttempt::Stale(outcome);
            }
            Err(error) => {
                outcome.fts_failed = true;
                tracing::warn!(
                    note_id = %committed.id,
                    error = %error,
                    "committed note FTS indexing failed"
                );
            }
        }
    }

    // Resolve every vector table before dispatching embedding work. A model
    // whose store cannot be opened gets its own diagnostic and does not prevent
    // healthy models from running.
    let mut embedding_slots = Vec::with_capacity(model_names.len());
    embedding_slots.resize_with(model_names.len(), || None);
    let mut join_set = tokio::task::JoinSet::new();
    let mut task_models = HashMap::new();
    let text: std::sync::Arc<str> = std::sync::Arc::from(embedding_text);
    let usage_ctx = crate::usage::current();
    let mut pending_vectors = Vec::new();
    for (index, model_name) in model_names.iter().enumerate() {
        if let Err(error) = runtime.vectors_for_model(token, model_name) {
            tracing::warn!(
                note_id = %committed.id,
                model = %model_name,
                error = %error,
                "bulk note vector store is unavailable"
            );
            outcome.failures.push(NoteReindexFailure {
                stage: "vector_store",
                model: model_name.clone(),
            });
            continue;
        }

        let rt = runtime.clone();
        let token = token.clone();
        let model = model_name.clone();
        let text = std::sync::Arc::clone(&text);
        let ctx = usage_ctx.clone();
        let handle = join_set.spawn(async move {
            let future =
                rt.embed_document_with_model_outcome_for_token(&token, &model, text.as_ref());
            let result = match ctx {
                Some(ctx) => crate::usage::scope(ctx, future).await,
                None => future.await,
            };
            (index, result)
        });
        task_models.insert(handle.id(), index);
    }

    while let Some(joined) = join_set.join_next_with_id().await {
        match joined {
            Ok((_task_id, (index, result))) => embedding_slots[index] = Some(result),
            Err(join_error) => {
                if let Some(index) = task_models.get(&join_error.id()).copied() {
                    embedding_slots[index] = Some(Err(RuntimeError::Internal(format!(
                        "embedding task failed: {join_error}"
                    ))));
                }
            }
        }
    }

    for (index, model_name) in model_names.iter().enumerate() {
        let Some(embedding) = embedding_slots[index].take() else {
            // No slot means vector-store resolution already emitted the exact
            // failure for this model.
            continue;
        };
        let embedding = match embedding {
            Ok(embedding) => embedding,
            Err(error) => {
                tracing::warn!(
                    note_id = %committed.id,
                    model = %model_name,
                    error = %error,
                    "committed note vector embedding failed"
                );
                outcome.failures.push(NoteReindexFailure {
                    stage: "embedding",
                    model: model_name.clone(),
                });
                continue;
            }
        };
        outcome.embedding_truncation.observe(&embedding);

        #[cfg(any(test, feature = "fault-injection"))]
        let injected = crate::operations::consume_vector_insert_fail_fault(&committed.namespace);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let injected = false;
        if injected {
            tracing::warn!(
                note_id = %committed.id,
                model = %model_name,
                "bulk note vector insert was deterministically failed"
            );
            outcome.failures.push(NoteReindexFailure {
                stage: "vector_insert",
                model: model_name.clone(),
            });
            continue;
        }
        if embedding.vector.iter().any(|value| !value.is_finite()) {
            tracing::warn!(
                note_id = %committed.id,
                model = %model_name,
                "bulk note vector embedding contained a non-finite value"
            );
            outcome.failures.push(NoteReindexFailure {
                stage: "vector_insert",
                model: model_name.clone(),
            });
            continue;
        }
        pending_vectors.push((model_name.clone(), embedding.vector));
    }

    if pending_vectors.is_empty() {
        // If every model failed before a guarded write, a concurrent update or
        // delete could otherwise escape detection after the FTS witness.
        if !model_names.is_empty() {
            let current: RuntimeResult<Option<Note>> = match runtime.notes(token) {
                Ok(notes) => notes
                    .get_note_including_deleted(committed.id)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            match current {
                Ok(Some(current)) if note_matches_committed_witness(&current, committed) => {}
                Ok(_) => return NoteRevisionIndexAttempt::Stale(outcome),
                Err(error) => {
                    tracing::warn!(
                        note_id = %committed.id,
                        error = %error,
                        "could not verify note revision after failed embedding fan-out"
                    );
                    outcome.fts_failed = true;
                }
            }
        }
    } else {
        let pending_models: Vec<String> = pending_vectors
            .iter()
            .map(|(model, _)| model.clone())
            .collect();
        match guarded_insert_note_vectors(runtime, committed, pending_vectors).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    note_id = %committed.id,
                    "note revision changed during embedding; skipped stale vector batch"
                );
                return NoteRevisionIndexAttempt::Stale(outcome);
            }
            Err(error) => {
                for model in pending_models {
                    tracing::warn!(
                        note_id = %committed.id,
                        model = %model,
                        error = %error,
                        "bulk note vector batch insert failed"
                    );
                    outcome.failures.push(NoteReindexFailure {
                        stage: "vector_insert",
                        model,
                    });
                }
            }
        }
    }

    NoteRevisionIndexAttempt::Complete(outcome)
}

/// Rebuild post-commit indexes while following a bounded number of concurrent
/// live revisions. Natural-key singleton/best-effort creates include FTS;
/// atomic creates already committed FTS with the row and skip vector work when
/// a newer revision owns index maintenance. All embedding tasks are awaited so
/// one failed model never cancels or hides a healthy sibling.
pub(crate) async fn index_committed_note_if_current(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    committed: &Note,
    embedding_text: &str,
    model_names: &[String],
    include_fts: bool,
) -> NoteReindexOutcome {
    const MAX_REVISION_ATTEMPTS: usize = 4;

    let mut ordered_model_names = model_names.to_vec();
    ordered_model_names.sort();
    let model_names = ordered_model_names.as_slice();
    let mut outcome = NoteReindexOutcome::default();

    // Atomic creates already own FTS transactionally, so their historical seam
    // remains before the cheap revision read. Natural-key creates pause after
    // that read, directly exercising the writer-transaction FTS witness.
    #[cfg(test)]
    if !include_fts {
        pause_before_note_post_commit_index(committed).await;
    }
    if !include_fts && model_names.is_empty() {
        return outcome;
    }

    #[cfg(test)]
    let mut natural_pause_pending = include_fts;
    let mut target = committed.clone();
    let mut target_embedding_text = embedding_text.to_string();
    let mut require_fts = include_fts;

    for attempt_index in 0..MAX_REVISION_ATTEMPTS {
        let current = match runtime.notes(token) {
            Ok(notes) => match notes.get_note_including_deleted(committed.id).await {
                Ok(Some(current)) if current.deleted_at.is_none() => current,
                Ok(_) => {
                    tracing::debug!(
                        note_id = %committed.id,
                        "committed note was deleted; abandoning post-commit index repair"
                    );
                    return outcome;
                }
                Err(error) => {
                    tracing::warn!(
                        note_id = %committed.id,
                        error = %error,
                        "post-commit index repair could not read the live note revision"
                    );
                    outcome.fts_failed |= require_fts;
                    for model in model_names {
                        outcome.failures.push(NoteReindexFailure {
                            stage: "vector_insert",
                            model: model.clone(),
                        });
                    }
                    return outcome;
                }
            },
            Err(error) => {
                tracing::warn!(
                    note_id = %committed.id,
                    error = %error,
                    "post-commit index repair could not access the note store"
                );
                outcome.fts_failed |= require_fts;
                for model in model_names {
                    outcome.failures.push(NoteReindexFailure {
                        stage: "vector_insert",
                        model: model.clone(),
                    });
                }
                return outcome;
            }
        };

        if !note_matches_committed_witness(&current, &target) {
            if !include_fts {
                // Atomic create committed its FTS row with the note. A newer
                // revision owns its own text/vector maintenance, so preserve
                // the historical coordinator contract and skip stale work
                // instead of duplicating that revision's embedding.
                tracing::debug!(
                    note_id = %committed.id,
                    "atomic note changed after commit; skipping stale post-commit vector work"
                );
                return outcome;
            }
            target_embedding_text =
                if current.content == committed.content && current.name == committed.name {
                    embedding_text.to_string()
                } else {
                    note_embedding_text_ref(&current).to_string()
                };
            target = current;
            // A non-text update does not reindex itself. Once the original
            // revision changes, this recovery path must own both substrates.
            require_fts = true;
        }

        #[cfg(test)]
        if natural_pause_pending {
            pause_before_note_post_commit_index(&target).await;
            natural_pause_pending = false;
        }

        match index_note_revision_once(
            runtime,
            token,
            &target,
            &target_embedding_text,
            model_names,
            require_fts,
        )
        .await
        {
            NoteRevisionIndexAttempt::Complete(indexed) => {
                outcome
                    .embedding_truncation
                    .merge(indexed.embedding_truncation);
                outcome.fts_failed |= indexed.fts_failed;
                outcome.failures.extend(indexed.failures);
                return outcome;
            }
            NoteRevisionIndexAttempt::Stale(indexed) => {
                // Work happened, so preserve truncation accounting, but discard
                // repair diagnostics for the superseded attempt. A successful
                // latest-revision retry leaves nothing for the caller to repair.
                outcome
                    .embedding_truncation
                    .merge(indexed.embedding_truncation);
                if !include_fts {
                    // The guarded vector transaction observed a concurrent
                    // mutation after embedding began. Atomic callers retain
                    // their vector-only skip semantics; the mutating path owns
                    // any replacement index work.
                    return outcome;
                }
                require_fts = true;
                tracing::debug!(
                    note_id = %committed.id,
                    attempt = attempt_index + 1,
                    "note revision changed during post-commit indexing; retrying latest revision"
                );
            }
        }
    }

    // Sustained mutation exhausted the bounded repair loop. The row remains
    // durable, and the existing closed repair stages communicate that both
    // substrates need a later rebuild without inventing a new wire value.
    outcome.fts_failed = true;
    for model in model_names {
        outcome.failures.push(NoteReindexFailure {
            stage: "vector_insert",
            model: model.clone(),
        });
    }
    outcome
}

/// Validate, build, and atomically persist a mixed entity/note batch.
pub async fn create_records_atomic(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    specs: Vec<BulkRecordCreateSpec>,
) -> RuntimeResult<BulkRecordCreateResult> {
    if specs.is_empty() {
        return Ok(BulkRecordCreateResult::default());
    }

    let namespace = token.namespace().as_str();
    let mut prepared = Vec::with_capacity(specs.len());

    // Validate and materialize every record before the first write.
    for spec in specs {
        match spec {
            BulkRecordCreateSpec::Entity(spec) => {
                runtime.validate_entity_kind(&spec.kind)?;
                let entity_type = runtime
                    .validate_entity_type_for_kind(&spec.kind, spec.entity_type.as_deref())?;
                if spec.name.trim().is_empty() {
                    return Err(RuntimeError::InvalidInput("name must not be empty".into()));
                }
                crate::secret_gate::check(&spec.name)?;
                if let Some(description) = &spec.description {
                    crate::secret_gate::check(description)?;
                }
                if let Some(properties) = &spec.properties {
                    crate::secret_gate::check_json(properties)?;
                }
                crate::secret_gate::check_tags(&spec.tags)?;

                let mut entity = Entity::new(namespace, &spec.kind, &spec.name)
                    .with_entity_type(entity_type.as_deref());
                if let Some(description) = spec.description {
                    entity = entity.with_description(description);
                }
                if let Some(properties) = spec.properties {
                    entity = entity.with_properties(properties);
                }
                if !spec.tags.is_empty() {
                    entity = entity.with_tags(spec.tags);
                }
                prepared.push(PreparedRecord::Entity(entity));
            }
            BulkRecordCreateSpec::Note(spec) => {
                runtime.validate_note_kind(&spec.kind)?;
                if spec.content.trim().is_empty() {
                    return Err(RuntimeError::InvalidInput(
                        "content must not be empty for note create".into(),
                    ));
                }
                let properties =
                    runtime.derive_note_write_properties(&spec.kind, token, spec.properties)?;
                crate::secret_gate::check(&spec.content)?;
                if let Some(name) = &spec.name {
                    crate::secret_gate::check(name)?;
                }
                if let Some(properties) = &properties {
                    crate::secret_gate::check_json(properties)?;
                }
                if let Some(salience) = spec.salience {
                    if !salience.is_finite() || !(0.0..=1.0).contains(&salience) {
                        return Err(RuntimeError::InvalidInput(format!(
                            "salience must be a finite value in [0.0, 1.0]; got {salience}"
                        )));
                    }
                }

                let property_external_id = properties_external_id(properties.as_ref())?;
                if property_external_id != spec.external_id {
                    return Err(RuntimeError::InvalidInput(
                        "bulk note external_id must match properties.external_id".into(),
                    ));
                }

                let mut note = Note::new(namespace, &spec.kind, &spec.content);
                if let Some(name) = spec.name {
                    note = note.with_name(name);
                }
                if let Some(salience) = spec.salience {
                    note = note.with_salience(salience);
                }
                if let Some(properties) = properties {
                    note = note.with_properties(properties);
                }
                prepared.push(PreparedRecord::Note {
                    note,
                    external_id: spec.external_id,
                });
            }
        }
    }

    // Resolve capability surfaces before opening the atomic writer.
    let _ = runtime.entities(token)?;
    let _ = runtime.notes(token)?;
    let _ = runtime.text(token)?;
    let _ = runtime.text_for_notes(token)?;

    let records_for_write = prepared.clone();
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let mut stored = Vec::with_capacity(records_for_write.len());
            for record in records_for_write {
                match record {
                    PreparedRecord::Entity(entity) => {
                        let affected = writer.execute(entity_upsert_statement(&entity)).await?;
                        if affected != 1 {
                            return Err(StorageError::Internal(format!(
                                "bulk entity insert affected {affected} rows; expected 1"
                            )));
                        }
                        writer
                            .execute(insert_document_statement(
                                "fts_entities",
                                &entity_fts_document(&entity),
                            ))
                            .await?;
                        stored.push(StoredOutcome {
                            record: BulkCreatedRecord::Entity(entity),
                            created: true,
                        });
                    }
                    PreparedRecord::Note { note, external_id } => {
                        let affected = writer
                            .execute(note_insert_or_ignore_statement(&note))
                            .await?;
                        match affected {
                            1 => {
                                writer.execute(note_assign_seq_statement(note.id)).await?;
                                writer
                                    .execute(insert_document_statement(
                                        "fts_notes",
                                        &note_fts_document(&note),
                                    ))
                                    .await?;
                                let stored_note = note_from_transaction_row(
                                    writer
                                        .query_row(note_transaction_row_by_id(note.id))
                                        .await?,
                                )?;
                                stored.push(StoredOutcome {
                                    record: BulkCreatedRecord::Note(stored_note),
                                    created: true,
                                });
                            }
                            0 => {
                                let Some(external_id) = external_id else {
                                    return Err(StorageError::Internal(
                                        "bulk note insert hit a non-natural-key constraint".into(),
                                    ));
                                };
                                let canonical = note_from_transaction_row(
                                    writer
                                        .query_row(canonical_note_transaction_row(
                                            &note,
                                            external_id,
                                        ))
                                        .await?,
                                )?;
                                stored.push(StoredOutcome {
                                    record: BulkCreatedRecord::Note(canonical),
                                    created: false,
                                });
                            }
                            other => {
                                return Err(StorageError::Internal(format!(
                                    "bulk note insert affected {other} rows; expected 0 or 1"
                                )));
                            }
                        }
                    }
                }
            }
            Ok(Box::new(stored) as Box<dyn Any + Send>)
        })
    });

    let boxed = runtime.sql().atomic_unit(op).await?;
    let stored = *boxed.downcast::<Vec<StoredOutcome>>().map_err(|_| {
        RuntimeError::Internal("bulk create atomic outcome had unexpected type".into())
    })?;

    #[cfg(test)]
    if let Some(note_id) = take_delete_note_after_atomic_commit(namespace) {
        let deleted = runtime
            .notes(token)?
            .delete_note(note_id, khive_storage::DeleteMode::Hard)
            .await?;
        if !deleted {
            return Err(RuntimeError::Internal(format!(
                "atomic-create test seam could not delete note {note_id}"
            )));
        }
    }

    let mut result = BulkRecordCreateResult {
        outcomes: Vec::with_capacity(stored.len()),
        ..Default::default()
    };
    for outcome in stored {
        match outcome.record {
            BulkCreatedRecord::Entity(entity) => {
                result.outcomes.push(BulkRecordCreateOutcome {
                    record: BulkCreatedRecord::Entity(entity),
                    created: outcome.created,
                });
            }
            BulkCreatedRecord::Note(note) => {
                if outcome.created {
                    let model_names = runtime.registered_embedding_model_names();
                    let indexed = index_committed_note_if_current(
                        runtime,
                        token,
                        &note,
                        note_embedding_text_ref(&note),
                        &model_names,
                        false,
                    )
                    .await;
                    result
                        .embedding_truncation
                        .merge(indexed.embedding_truncation);
                    if !indexed.failures.is_empty() {
                        result.post_commit_failures.push(BulkPostCommitFailure {
                            note_id: note.id,
                            stages: indexed
                                .failures
                                .into_iter()
                                .map(|failure| BulkPostCommitFailureStage {
                                    stage: failure.stage.to_string(),
                                    model: Some(failure.model),
                                })
                                .collect(),
                        });
                    }
                    runtime.fire_note_mutation_hook(&note.kind, note.id).await;
                }
                result.outcomes.push(BulkRecordCreateOutcome {
                    record: BulkCreatedRecord::Note(note),
                    created: outcome.created,
                });
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::{Arc, Mutex};

    use crate::curation::NotePatch;
    use crate::embedder_registry::EmbedderProvider;
    use crate::operations::arm_vector_fail_scoped;
    use crate::RuntimeConfig;

    const MODEL: &str = "atomic-create-repair-model";
    const DIMS: usize = 4;

    struct StubService {
        inputs: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbeddingService for StubService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.inputs
                .lock()
                .expect("stub embedding input mutex poisoned")
                .extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![0.5; DIMS]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            MODEL
        }
    }

    struct StubProvider {
        inputs: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbedderProvider for StubProvider {
        fn name(&self) -> &str {
            MODEL
        }

        fn dimensions(&self) -> usize {
            DIMS
        }

        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(StubService {
                inputs: Arc::clone(&self.inputs),
            }))
        }
    }

    fn register_stub(runtime: &KhiveRuntime) -> Arc<Mutex<Vec<String>>> {
        let inputs = Arc::new(Mutex::new(Vec::new()));
        runtime.register_embedder(StubProvider {
            inputs: Arc::clone(&inputs),
        });
        inputs
    }

    #[tokio::test]
    async fn committed_note_vector_failure_returns_structured_repair_diagnostic() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        register_stub(&runtime);
        let namespace = Namespace::parse("atomic-create-vector-repair").expect("namespace");
        let token = NamespaceToken::for_namespace(namespace.clone());
        let _arm = arm_vector_fail_scoped(namespace.as_str());

        let result = create_records_atomic(
            &runtime,
            &token,
            vec![BulkRecordCreateSpec::Note(BulkNoteCreateSpec {
                kind: "observation".to_string(),
                name: None,
                content: "durable note with a failed vector side effect".to_string(),
                salience: None,
                properties: None,
                external_id: None,
            })],
        )
        .await
        .expect("the durable row survives a post-commit vector failure");

        assert_eq!(result.outcomes.len(), 1);
        assert!(result.outcomes[0].created);
        let note_id = result.outcomes[0].record.id();
        assert_eq!(
            result.post_commit_failures,
            vec![BulkPostCommitFailure {
                note_id,
                stages: vec![BulkPostCommitFailureStage {
                    stage: "vector_insert".to_string(),
                    model: Some(MODEL.to_string()),
                }],
            }]
        );
        let details = serde_json::to_value(&result.post_commit_failures)
            .expect("repair diagnostics serialize for the public response");
        assert_eq!(details[0]["id"], note_id.to_string());
        assert_eq!(details[0]["stages"][0]["stage"], "vector_insert");
        assert_eq!(details[0]["stages"][0]["model"], MODEL);
        assert!(runtime
            .notes(&token)
            .expect("note store")
            .get_note(note_id)
            .await
            .expect("read note")
            .is_some());
    }

    #[tokio::test]
    async fn natural_key_retry_returns_the_transaction_snapshot_after_commit_race() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        let namespace = Namespace::parse("atomic-create-transaction-snapshot").expect("namespace");
        let token = NamespaceToken::for_namespace(namespace.clone());
        let external_id = "transaction-snapshot-key";

        let initial = create_records_atomic(
            &runtime,
            &token,
            vec![BulkRecordCreateSpec::Note(BulkNoteCreateSpec {
                kind: "observation".to_string(),
                name: Some("canonical name".to_string()),
                content: "canonical content captured by the writer transaction".to_string(),
                salience: Some(0.75),
                properties: Some(serde_json::json!({
                    "external_id": external_id,
                    "version": "canonical"
                })),
                external_id: Some(external_id.to_string()),
            })],
        )
        .await
        .expect("initial natural-key note");
        let canonical = match &initial.outcomes[0].record {
            BulkCreatedRecord::Note(note) => note.clone(),
            BulkCreatedRecord::Entity(_) => panic!("expected note outcome"),
        };
        assert!(initial.outcomes[0].created);

        arm_delete_note_after_atomic_commit(namespace.as_str(), canonical.id);
        let retry = create_records_atomic(
            &runtime,
            &token,
            vec![BulkRecordCreateSpec::Note(BulkNoteCreateSpec {
                kind: "observation".to_string(),
                name: Some("retry name".to_string()),
                content: "retry content must not replace the canonical row".to_string(),
                salience: Some(0.1),
                properties: Some(serde_json::json!({
                    "external_id": external_id,
                    "version": "retry"
                })),
                external_id: Some(external_id.to_string()),
            })],
        )
        .await
        .expect("retry returns the row captured before the post-commit delete");

        assert!(!retry.outcomes[0].created);
        let returned = match &retry.outcomes[0].record {
            BulkCreatedRecord::Note(note) => note,
            BulkCreatedRecord::Entity(_) => panic!("expected note outcome"),
        };
        assert_eq!(returned.id, canonical.id);
        assert_eq!(returned.name, canonical.name);
        assert_eq!(returned.content, canonical.content);
        assert_eq!(returned.salience, canonical.salience);
        assert_eq!(returned.properties, canonical.properties);
        assert_eq!(returned.created_at, canonical.created_at);
        assert!(runtime
            .notes(&token)
            .expect("note store")
            .get_note(canonical.id)
            .await
            .expect("read note")
            .is_none());
    }

    #[tokio::test]
    async fn post_commit_update_preserves_new_fts_and_skips_stale_vector_work() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        let inputs = register_stub(&runtime);
        let namespace = Namespace::parse("atomic-create-post-commit-update").expect("namespace");
        let token = NamespaceToken::for_namespace(namespace.clone());
        let (committed, resume) = arm_post_commit_pause(namespace.as_str());
        let create_runtime = runtime.clone();
        let create_token = token.clone();

        let create = tokio::spawn(async move {
            create_records_atomic(
                &create_runtime,
                &create_token,
                vec![BulkRecordCreateSpec::Note(BulkNoteCreateSpec {
                    kind: "observation".to_string(),
                    name: None,
                    content: "stale pre-update content".to_string(),
                    salience: None,
                    properties: None,
                    external_id: None,
                })],
            )
            .await
        });
        let note_id = tokio::time::timeout(std::time::Duration::from_secs(10), committed)
            .await
            .expect("atomic commit reaches the pause before timeout")
            .expect("atomic commit pause sender stays live");
        let updated = runtime
            .update_note(
                &token,
                note_id,
                NotePatch::new(
                    None,
                    Some("current post-update content".to_string()),
                    None,
                    None,
                    None,
                ),
            )
            .await
            .expect("concurrent update commits while vector work is paused");
        resume.send(()).expect("resume atomic create");
        let result = create
            .await
            .expect("create task joins")
            .expect("create returns its committed outcome");

        assert!(result.post_commit_failures.is_empty());
        let current = runtime
            .notes(&token)
            .expect("note store")
            .get_note(note_id)
            .await
            .expect("read current note")
            .expect("updated note remains live");
        assert_eq!(current.updated_at, updated.updated_at);
        assert_eq!(current.content, "current post-update content");
        let fts = runtime
            .text_for_notes(&token)
            .expect("note FTS")
            .get_document(namespace.as_str(), note_id)
            .await
            .expect("read FTS document")
            .expect("updated note stays indexed");
        assert_eq!(fts.body, "current post-update content");
        assert_eq!(fts.updated_at.timestamp_micros(), updated.updated_at);
        assert_eq!(
            inputs
                .lock()
                .expect("stub embedding input mutex poisoned")
                .as_slice(),
            &["current post-update content".to_string()],
            "the stale committed revision must never reach the embedder"
        );
        assert_eq!(
            runtime
                .vectors_for_model(&token, MODEL)
                .expect("vector store")
                .count()
                .await
                .expect("vector count"),
            1,
            "the update's current vector must remain present"
        );
    }

    #[tokio::test]
    async fn post_commit_soft_and_hard_delete_do_not_resurrect_indexes() {
        for hard in [false, true] {
            let runtime = KhiveRuntime::new(RuntimeConfig {
                db_path: None,
                packs: vec!["kg".to_string()],
                ..RuntimeConfig::no_embeddings()
            })
            .expect("runtime");
            let inputs = register_stub(&runtime);
            let namespace = Namespace::parse(&format!(
                "atomic-create-post-commit-{}-delete",
                if hard { "hard" } else { "soft" }
            ))
            .expect("namespace");
            let token = NamespaceToken::for_namespace(namespace.clone());
            let (committed, resume) = arm_post_commit_pause(namespace.as_str());
            let create_runtime = runtime.clone();
            let create_token = token.clone();

            let create = tokio::spawn(async move {
                create_records_atomic(
                    &create_runtime,
                    &create_token,
                    vec![BulkRecordCreateSpec::Note(BulkNoteCreateSpec {
                        kind: "observation".to_string(),
                        name: None,
                        content: "must not be indexed after delete".to_string(),
                        salience: None,
                        properties: None,
                        external_id: None,
                    })],
                )
                .await
            });
            let note_id = tokio::time::timeout(std::time::Duration::from_secs(10), committed)
                .await
                .expect("atomic commit reaches the pause before timeout")
                .expect("atomic commit pause sender stays live");
            assert!(runtime
                .delete_note(&token, note_id, hard)
                .await
                .expect("concurrent delete succeeds"));
            resume.send(()).expect("resume atomic create");
            let result = create
                .await
                .expect("create task joins")
                .expect("create returns its committed outcome");

            assert!(result.post_commit_failures.is_empty());
            let stored = runtime
                .notes(&token)
                .expect("note store")
                .get_note_including_deleted(note_id)
                .await
                .expect("read note including deleted");
            if hard {
                assert!(stored.is_none(), "hard-deleted row must stay absent");
            } else {
                assert!(
                    stored.is_some_and(|note| note.deleted_at.is_some()),
                    "soft-deleted row must stay tombstoned"
                );
            }
            assert!(runtime
                .text_for_notes(&token)
                .expect("note FTS")
                .get_document(namespace.as_str(), note_id)
                .await
                .expect("read FTS document")
                .is_none());
            assert_eq!(
                runtime
                    .vectors_for_model(&token, MODEL)
                    .expect("vector store")
                    .count()
                    .await
                    .expect("vector count"),
                0,
                "deleted note must not regain a vector"
            );
            assert!(
                inputs
                    .lock()
                    .expect("stub embedding input mutex poisoned")
                    .is_empty(),
                "deleted committed content must not reach the embedder"
            );
        }
    }
}
