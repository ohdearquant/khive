//! Ordered note streams. Embedding preparation precedes one SQL-only writer
//! transaction containing the sequence predicates, note/index writes and ledger
//! rows. A batch is that transaction over several members (atomic mode) or one
//! such transaction per member, in list order (per-member mode).
use std::any::Any;
use std::collections::{HashMap, HashSet};

use khive_storage::{
    AtomicUnitOp, Note, SqlAccess, SqlRow, SqlStatement, SqlValue, SqlWriter, StorageCapability,
    StorageError,
};
use khive_types::{Details, KhiveError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::atomic_message::{
    prepare_atomic_note_requests, prepare_atomic_notes, AtomicNoteOptions, AtomicNoteRequest,
    AtomicNoteSpec,
};
use crate::atomic_plan::{PlanStatement, PostCommitEffect};
use crate::atomic_runner::{
    apply_plan, run_prepared_atomic_unit, AtomicOpFailure, AtomicOpPlan,
    CommittedPostCommitEffects, PreparedAtomicError, PreparedAtomicOp, PreparedAtomicOutcome,
};
use crate::note_write::{
    NoteFence, NoteFences, NoteWriteConflict, NoteWriteGuard, NoteWriteOptions,
};
use crate::{
    micros_to_iso, DomainDisposition, KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult,
    VerbRegistry,
};

fn statement(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: Some("stream".into()),
    }
}

fn validate_stream(stream: &str) -> RuntimeResult<()> {
    if stream.len() > 512 || stream.contains('\0') {
        return Err(RuntimeError::InvalidInput(
            "stream must be at most 512 UTF-8 bytes and contain no U+0000".into(),
        ));
    }
    Ok(())
}

fn integer(row: &SqlRow, name: &str) -> RuntimeResult<i64> {
    match row.get(name) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        _ => Err(RuntimeError::Internal(format!(
            "stream query missing integer {name}"
        ))),
    }
}

fn text<'a>(row: &'a SqlRow, name: &str) -> RuntimeResult<&'a str> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Ok(value),
        _ => Err(RuntimeError::Internal(format!(
            "stream query missing text {name}"
        ))),
    }
}

fn write_failure(message: &str) -> StorageError {
    StorageError::Conflict {
        capability: StorageCapability::Notes,
        operation: "stream.append".into(),
        message: message.into(),
    }
}

/// One append, shape-validated by the verb layer; the runtime validates the
/// stream name and the record's serialization before any write.
pub struct StreamAppendSpec {
    pub stream: String,
    pub record: Value,
    pub expected_seq: Option<i64>,
    pub note_kind: String,
    pub tags: Option<Vec<String>>,
    pub fence: Option<NoteFences>,
}

/// A keyed document write. Kinds are canonical note-kind names. A missing
/// expected version creates only; a positive version updates only.
#[derive(Clone, Debug)]
pub struct StreamWriteSpec {
    pub key: String,
    pub kind: String,
    pub doc: Value,
    pub tags: Option<Vec<String>>,
    pub embed: Option<bool>,
    pub expected_version: Option<i64>,
}

/// An exact live-key observation; `None` asserts that the key is unheld.
#[derive(Clone, Debug)]
pub struct StreamObservation {
    pub key: String,
    pub kind: String,
    pub version: Option<i64>,
    /// Dotted document path whose RFC 3339 value must exceed the writer clock.
    pub live_until: Option<String>,
}

/// A batch member or its already established refusal. The mode places it.
pub enum StreamBatchMember {
    Append(StreamAppendSpec),
    Write(StreamWriteSpec),
    Refused(KhiveError),
}

/// The member refusal that stopped an atomic batch; nothing was written.
#[derive(Debug)]
pub struct StreamBatchRefusal {
    pub member: usize,
    pub error: KhiveError,
}

/// A member refusal as a value: the error object a refused op carries, plus
/// the disposition the consumer rule reads without a special case.
pub fn refusal_value(error: &KhiveError) -> RuntimeResult<Value> {
    let mut value = serde_json::to_value(error)
        .map_err(|e| RuntimeError::Internal(format!("stream refusal serialization: {e}")))?;
    value["domain_disposition"] = json!(DomainDisposition::NotCommitted.as_str());
    Ok(value)
}

fn seq_conflict(stream: &str, expected: i64, next: i64, member: Option<usize>) -> KhiveError {
    let mut pairs = vec![
        ("reason", "seq_conflict".to_string()),
        ("stream", stream.to_string()),
        ("expected_seq", expected.to_string()),
        ("next_seq", next.to_string()),
    ];
    if let Some(member) = member {
        pairs.push(("member", member.to_string()));
    }
    KhiveError::conflict("stream sequence precondition failed")
        .with_details(Details::new_owned(pairs))
}

/// A prepared append: the note, its planned statements and the ledger key.
struct PreparedAppend {
    stream: String,
    expected_seq: Option<i64>,
    note: Note,
    statements: Vec<PlanStatement>,
    guard: NoteWriteGuard,
}

fn append_result(prepared: &PreparedAppend, seq: i64) -> Value {
    json!({"seq": seq, "id": prepared.note.id, "created_at": micros_to_iso(prepared.note.created_at)})
}

enum BatchOutcome {
    Appended(Vec<i64>),
    Conflict { next: i64 },
    FenceConflict { conflict: NoteWriteConflict },
}

struct PreparedBatchMember {
    index: usize,
    action: PreparedBatchAction,
}

enum PreparedBatchAction {
    Append {
        stream: String,
        expected_seq: Option<i64>,
        note: Box<Note>,
        plan: AtomicOpPlan,
        fence: Option<NoteFences>,
    },
    Write {
        key: String,
        kind: String,
        id: Uuid,
        plan: AtomicOpPlan,
        after_create: Option<Value>,
    },
    Refused(KhiveError),
}

#[derive(Deserialize)]
struct StreamCreateFields {
    content: String,
    name: Option<String>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
    salience: Option<f64>,
    embed: Option<bool>,
}

struct PreparedStreamCreate {
    key: String,
    kind: String,
    fields: StreamCreateFields,
    args: Value,
}

enum StreamBatchPreparation {
    Ready(Box<PreparedBatchAction>),
    Append(Box<(StreamAppendSpec, String)>),
    Create(Box<PreparedStreamCreate>),
}

fn batch_create_hooks(members: &[PreparedBatchMember]) -> Vec<(Uuid, String, Value)> {
    members
        .iter()
        .filter_map(|member| match &member.action {
            PreparedBatchAction::Write {
                id,
                kind,
                after_create: Some(args),
                ..
            } => Some((*id, kind.clone(), args.clone())),
            _ => None,
        })
        .collect()
}

fn place_member(error: KhiveError, member: Option<usize>) -> RuntimeResult<KhiveError> {
    let pairs = member
        .into_iter()
        .map(|member| ("member".to_owned(), member.to_string()))
        .chain(
            error
                .details()
                .into_iter()
                .flat_map(Details::iter)
                .filter(|(key, _)| *key != "member")
                .map(|(key, value)| (key.to_owned(), value.to_owned())),
        );
    let details = Details::deserialize(serde::de::value::MapDeserializer::<
        _,
        serde::de::value::Error,
    >::new(pairs))
    .map_err(|error| RuntimeError::Internal(format!("stream member details: {error}")))?;
    Ok(error.with_details(details))
}

fn missing_write(key: &str) -> KhiveError {
    KhiveError::not_found("note key", key).with_details(Details::new_owned([
        ("reason", "stream_write_not_found".into()),
        ("key", key.into()),
    ]))
}

async fn check_observed(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    observed: &[StreamObservation],
    now: Option<i64>,
) -> Result<Option<KhiveError>, StorageError> {
    for (index, entry) in observed.iter().enumerate() {
        let current = writer.query_scalar(SqlStatement {
            sql: "SELECT version FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL".into(),
            params: vec![SqlValue::Text(namespace.into()), SqlValue::Text(entry.kind.clone()), SqlValue::Text(entry.key.clone())],
            label: Some("stream-batch-observed".into()),
        }).await?;
        let current = match current {
            None => None,
            Some(SqlValue::Integer(version)) => Some(version),
            Some(_) => {
                return Err(StorageError::Internal(
                    "invalid observed note version".into(),
                ))
            }
        };
        if current != entry.version {
            let mut details = vec![
                ("reason", "version_conflict".into()),
                ("key", entry.key.clone()),
                ("index", index.to_string()),
            ];
            if let Some(expected) = entry.version {
                details.push(("expected_version", expected.to_string()));
            }
            if let Some(current) = current {
                details.push(("current_version", current.to_string()));
            }
            return Ok(Some(
                KhiveError::conflict("stream observation precondition failed")
                    .with_details(Details::new_owned(details)),
            ));
        }
        if let Some(field) = &entry.live_until {
            let content = writer.query_scalar(SqlStatement {
                sql: "SELECT content FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL".into(),
                params: vec![SqlValue::Text(namespace.into()), SqlValue::Text(entry.kind.clone()), SqlValue::Text(entry.key.clone())],
                label: Some("stream-batch-live-until".into()),
            }).await?;
            let doc: Value = match content {
                Some(SqlValue::Text(content)) => {
                    serde_json::from_str(&content).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            };
            let found = field
                .split('.')
                .try_fold(&doc, |value, part| value.get(part));
            let value = found.unwrap_or(&Value::Null);
            let deadline = value
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
            let now = now
                .ok_or_else(|| StorageError::Internal("missing stream observation clock".into()))?;
            let clock = chrono::DateTime::from_timestamp_micros(now)
                .ok_or_else(|| StorageError::Internal("invalid stream observation clock".into()))?;
            let reason = match deadline {
                None => Some("live_until_unreadable"),
                Some(deadline) if deadline <= clock => Some("expired"),
                Some(_) => None,
            };
            if let Some(reason) = reason {
                let mut details = vec![
                    ("reason", reason.into()),
                    ("key", entry.key.clone()),
                    ("kind", entry.kind.clone()),
                    ("version", entry.version.unwrap().to_string()),
                    ("field", field.clone()),
                    ("index", index.to_string()),
                ];
                if reason == "expired" {
                    // Only a value that parsed as RFC 3339 is echoed, because that value is
                    // the deadline the caller pinned. The path is caller-chosen, so echoing
                    // whatever it lands on would read any field of the document back out.
                    details.push(("value", value.to_string()));
                    details.push(("now", micros_to_iso(now)));
                } else {
                    details.push(("value_type", found.map_or("absent", json_type_name).into()));
                }
                return Ok(Some(
                    KhiveError::conflict("stream observation time precondition failed")
                        .with_details(Details::new_owned(details)),
                ));
            }
        }
    }
    Ok(None)
}

/// The JSON type of a `live_until` field, which an unreadable refusal reports in
/// place of the value itself.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

async fn observation_clock(writer: &mut dyn SqlWriter) -> Result<i64, StorageError> {
    match writer
        .query_scalar(SqlStatement {
            sql: "SELECT khive_now_micros()".into(),
            params: vec![],
            label: Some("stream-batch-clock".into()),
        })
        .await?
    {
        Some(SqlValue::Integer(now)) => Ok(now),
        _ => Err(StorageError::Internal(
            "invalid stream observation clock".into(),
        )),
    }
}

struct BatchFailure {
    member: Option<usize>,
    error: RuntimeError,
}

async fn run_prepared_stream_batch(
    access: &dyn SqlAccess,
    namespace: String,
    members: Vec<PreparedBatchMember>,
    fence: Option<NoteFence>,
    observed: Vec<StreamObservation>,
) -> RuntimeResult<Result<(Vec<Value>, CommittedPostCommitEffects), StreamBatchRefusal>> {
    let op: PreparedAtomicOp<Vec<Value>, BatchFailure> = Box::new(move |writer| {
        Box::pin(async move {
            // One SQL clock read after writer admission, shared by the list.
            let now = if observed.iter().any(|entry| entry.live_until.is_some()) {
                Some(observation_clock(writer).await?)
            } else {
                None
            };
            let guard = NoteWriteGuard {
                namespace: namespace.clone(),
                target_id: Uuid::nil(),
                expected_version: None,
                fence: fence.map(Into::into),
                create_key: None,
            };
            let predicate_error = if let Some(conflict) = guard.check_fence(writer).await? {
                Some(conflict.into_error())
            } else {
                check_observed(writer, &namespace, &observed, now).await?
            };
            if let Some(error) = predicate_error {
                return Err(PreparedAtomicError::Refused {
                    failure: BatchFailure {
                        member: None,
                        error: error.into(),
                    },
                    message: "stream batch predicate refused".into(),
                });
            }
            let mut results = Vec::with_capacity(members.len());
            let mut effects = Vec::new();
            let mut heads = HashMap::new();
            // Member fences observe the transaction's initial state, even when
            // an earlier keyed write changes a fenced note in this batch.
            for member in &members {
                if let PreparedBatchAction::Append { fence, .. } = &member.action {
                    let guard = NoteWriteGuard {
                        namespace: namespace.clone(),
                        target_id: Uuid::nil(),
                        expected_version: None,
                        fence: fence.clone(),
                        create_key: None,
                    };
                    if let Some(conflict) = guard.check_fence(writer).await? {
                        return Err(PreparedAtomicError::Refused {
                            failure: BatchFailure {
                                member: Some(member.index),
                                error: conflict.into_error().into(),
                            },
                            message: "stream append fence refused".into(),
                        });
                    }
                }
            }
            for member in members {
                let result =
                    apply_stream_member(writer, &namespace, &mut heads, member.action).await;
                match result {
                    Ok((value, effect)) => {
                        results.push(value);
                        if let Some(effect) = effect {
                            effects.push(effect);
                        }
                    }
                    Err(error) => {
                        return Err(PreparedAtomicError::Refused {
                            failure: BatchFailure {
                                member: Some(member.index),
                                error,
                            },
                            message: "stream batch member refused".into(),
                        });
                    }
                }
            }
            Ok((results, effects))
        })
    });
    match run_prepared_atomic_unit(access, op).await? {
        PreparedAtomicOutcome::Committed { value, post_commit } => Ok(Ok((value, post_commit))),
        PreparedAtomicOutcome::RolledBack(BatchFailure {
            member: Some(member),
            error: RuntimeError::Khive(error),
        }) => Ok(Err(StreamBatchRefusal {
            member,
            error: place_member(error, Some(member))?,
        })),
        PreparedAtomicOutcome::RolledBack(BatchFailure { error, .. }) => Err(error),
    }
}

enum SequenceRefusal {
    Exhausted,
    Conflict { expected: i64, next: i64 },
}

fn allocate_sequence(head: &mut i64, expected: Option<i64>) -> Result<i64, SequenceRefusal> {
    let next = head.checked_add(1).ok_or(SequenceRefusal::Exhausted)?;
    if let Some(expected) = expected.filter(|expected| *expected != next) {
        return Err(SequenceRefusal::Conflict { expected, next });
    }
    *head = next;
    Ok(next)
}

async fn insert_stream_entry(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    stream: String,
    seq: i64,
    note_id: String,
) -> Result<(), StorageError> {
    writer
        .execute(statement(
            "INSERT INTO note_streams(namespace,stream,seq,note_id) VALUES (?1,?2,?3,?4)",
            vec![
                SqlValue::Text(namespace.into()),
                SqlValue::Text(stream),
                SqlValue::Integer(seq),
                SqlValue::Text(note_id),
            ],
        ))
        .await?;
    Ok(())
}

async fn apply_stream_member(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    heads: &mut HashMap<String, i64>,
    action: PreparedBatchAction,
) -> RuntimeResult<(Value, Option<PostCommitEffect>)> {
    match action {
        PreparedBatchAction::Refused(error) => Err(error.into()),
        PreparedBatchAction::Append {
            stream,
            expected_seq,
            note,
            plan,
            ..
        } => {
            let head = match heads.entry(stream.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let head = writer.query_scalar(statement(
                        "SELECT COALESCE(MAX(seq), 0) FROM note_streams WHERE namespace=?1 AND stream=?2",
                        vec![SqlValue::Text(namespace.into()), SqlValue::Text(stream.clone())],
                    )).await?;
                    let Some(SqlValue::Integer(head)) = head else {
                        return Err(RuntimeError::Internal("invalid stream head".into()));
                    };
                    entry.insert(head)
                }
            };
            let next = allocate_sequence(head, expected_seq).map_err(|refusal| match refusal {
                SequenceRefusal::Exhausted => {
                    RuntimeError::InvalidInput("stream sequence exhausted".into())
                }
                SequenceRefusal::Conflict { expected, next } => {
                    seq_conflict(&stream, expected, next, None).into()
                }
            })?;
            let applied = apply_plan(writer, &plan, false).await.map_err(|error| {
                RuntimeError::Internal(format!("stream append plan failed: {error:?}"))
            })?;
            insert_stream_entry(writer, namespace, stream, next, note.id.to_string()).await?;
            Ok((
                json!({"seq": next, "id": note.id, "created_at": micros_to_iso(note.created_at)}),
                applied.effect,
            ))
        }
        PreparedBatchAction::Write {
            key,
            kind,
            id,
            plan,
            ..
        } => match apply_plan(writer, &plan, false).await {
            Ok(applied) => {
                // Read the row under this writer, before commit or any later
                // writer. Preserve its existing timestamp/CAS semantics.
                let stored = writer
                    .query_row(SqlStatement {
                        sql: "SELECT version, updated_at FROM notes WHERE namespace=?1 AND id=?2"
                            .into(),
                        params: vec![
                            SqlValue::Text(namespace.into()),
                            SqlValue::Text(id.to_string()),
                        ],
                        label: Some("stream-batch-write-time".into()),
                    })
                    .await?;
                let Some(stored) = stored else {
                    return Err(RuntimeError::Internal(
                        "missing stream write timestamp".into(),
                    ));
                };
                Ok((
                    json!({"id": id, "version": integer(&stored, "version")?, "updated_at": micros_to_iso(integer(&stored, "updated_at")?)}),
                    applied.effect,
                ))
            }
            Err(AtomicOpFailure::NoteConflict(conflict)) => Err(conflict.into_error().into()),
            Err(AtomicOpFailure::GuardFailed { .. }) => {
                let holder = writer.query_row(statement(
                        "SELECT id, version FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL",
                        vec![SqlValue::Text(namespace.into()), SqlValue::Text(kind), SqlValue::Text(key.clone())],
                    )).await?;
                let Some(holder) = holder else {
                    return Err(missing_write(&key).into());
                };
                // Versions are local to a note identity. A replacement can be
                // at the expected version without being the prepared target.
                if text(&holder, "id")? != id.to_string() {
                    if let AtomicOpPlan::Update(update) = &plan {
                        if let Some(expected) = update
                            .note_guard
                            .as_ref()
                            .and_then(|guard| guard.expected_version)
                        {
                            return Err(NoteWriteConflict::Version {
                                expected,
                                current: integer(&holder, "version")?,
                            }
                            .into_error()
                            .into());
                        }
                    }
                }
                Err(crate::curation::stale_note_snapshot_error(id))
            }
            Err(error) => Err(RuntimeError::Internal(format!(
                "stream write plan failed: {error:?}"
            ))),
        },
    }
}

impl KhiveRuntime {
    /// Validate every spec and prepare every note before any write. All
    /// allocations for embedding, note normalization and SQL plans complete
    /// here, so only bounded statement driving occurs while the writer is held.
    async fn prepare_stream_appends(
        &self,
        token: &NamespaceToken,
        specs: &[&StreamAppendSpec],
    ) -> RuntimeResult<Vec<PreparedAppend>> {
        let mut contents = Vec::with_capacity(specs.len());
        for spec in specs {
            validate_stream(&spec.stream)?;
            if let Some(fences) = &spec.fence {
                fences.validate()?;
                for fence in fences.entries() {
                    self.validate_note_kind(&fence.kind)?;
                }
            }
            contents.push(
                serde_json::to_string(&spec.record)
                    .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?,
            );
        }
        let atomic_specs = specs
            .iter()
            .zip(&contents)
            .map(|(spec, content)| AtomicNoteSpec {
                token,
                id: None,
                kind: &spec.note_kind,
                name: None,
                content,
                properties: spec.tags.clone().map(|tags| json!({"tags": tags})),
            })
            .collect();
        let prepared =
            prepare_atomic_notes(self, atomic_specs, AtomicNoteOptions::default()).await?;
        let mut out = Vec::with_capacity(specs.len());
        for ((spec, note), plan) in specs.iter().zip(prepared.notes).zip(prepared.plans) {
            let AtomicOpPlan::AddNote(plan) = plan else {
                return Err(RuntimeError::Internal(
                    "stream preparation did not produce a note".into(),
                ));
            };
            let mut guard = plan.note_guard.ok_or_else(|| {
                RuntimeError::Internal("stream preparation did not produce a note guard".into())
            })?;
            guard.fence = spec.fence.clone();
            out.push(PreparedAppend {
                stream: spec.stream.clone(),
                expected_seq: spec.expected_seq,
                note,
                statements: plan.statements,
                guard,
            });
        }
        Ok(out)
    }

    /// Run prepared appends as one writer transaction. Every stream head is
    /// read, every number assigned in list order and every `expected_seq`
    /// checked before the first write; then notes and ledger rows land in
    /// list order, so appends to one stream take consecutive numbers.
    async fn run_stream_appends(
        &self,
        token: &NamespaceToken,
        appends: &[PreparedAppend],
    ) -> RuntimeResult<BatchOutcome> {
        let ns = token.namespace().as_str().to_string();
        let entries: Vec<_> = appends
            .iter()
            .map(|a| {
                (
                    a.stream.clone(),
                    a.expected_seq,
                    a.note.id.to_string(),
                    a.statements.clone(),
                    a.guard.clone(),
                )
            })
            .collect();
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let mut heads: Vec<(String, i64)> = Vec::new();
                for (stream, _, _, _, _) in &entries {
                    if heads.iter().any(|(known, _)| known == stream) {
                        continue;
                    }
                    let head = writer
                        .query_scalar(statement(
                            "SELECT COALESCE(MAX(seq), 0) FROM note_streams WHERE namespace=?1 AND stream=?2",
                            vec![SqlValue::Text(ns.clone()), SqlValue::Text(stream.clone())],
                        ))
                        .await?;
                    let Some(SqlValue::Integer(head)) = head else {
                        return Err(write_failure("invalid stream head"));
                    };
                    heads.push((stream.clone(), head));
                }
                let mut assigned = Vec::with_capacity(entries.len());
                for (stream, expected_seq, _, _, guard) in &entries {
                    if let Some(conflict) = guard.check_fence(writer).await? {
                        return Ok(Box::new(BatchOutcome::FenceConflict { conflict })
                            as Box<dyn Any + Send>);
                    }
                    let head = heads
                        .iter_mut()
                        .find(|(known, _)| known == stream)
                        .map(|(_, head)| head)
                        .ok_or_else(|| write_failure("stream head missing"))?;
                    let next = match allocate_sequence(head, *expected_seq) {
                        Ok(next) => next,
                        Err(SequenceRefusal::Exhausted) => {
                            return Err(write_failure("stream sequence exhausted"));
                        }
                        Err(SequenceRefusal::Conflict { next, .. }) => {
                            return Ok(
                                Box::new(BatchOutcome::Conflict { next }) as Box<dyn Any + Send>
                            );
                        }
                    };
                    assigned.push(next);
                }
                for ((stream, _, note_id, statements, _), seq) in
                    entries.into_iter().zip(assigned.iter().copied())
                {
                    for planned in statements {
                        let affected = writer.execute(planned.statement).await?;
                        if planned
                            .guard
                            .is_some_and(|guard| !guard.holds_for(affected))
                        {
                            return Err(write_failure("prepared note write guard failed"));
                        }
                    }
                    insert_stream_entry(writer, &ns, stream, seq, note_id).await?;
                }
                Ok(Box::new(BatchOutcome::Appended(assigned)) as Box<dyn Any + Send>)
            })
        });
        let outcome = self
            .sql()
            .atomic_unit(op)
            .await?
            .downcast::<BatchOutcome>()
            .map_err(|_| RuntimeError::Internal("invalid stream append outcome".into()))?;
        Ok(*outcome)
    }

    /// Append a JSON value as an immutable note. The sequence precondition and
    /// every note/index/ledger statement share the same writer transaction.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_append(
        &self,
        token: &NamespaceToken,
        stream: &str,
        record: &Value,
        expected_seq: Option<i64>,
        note_kind: &str,
        tags: Option<Vec<String>>,
        fence: Option<NoteFences>,
    ) -> RuntimeResult<Value> {
        let spec = StreamAppendSpec {
            stream: stream.to_string(),
            record: record.clone(),
            expected_seq,
            note_kind: note_kind.to_string(),
            tags,
            fence,
        };
        let prepared = self.prepare_stream_appends(token, &[&spec]).await?;
        match self.run_stream_appends(token, &prepared).await? {
            BatchOutcome::Appended(seqs) => Ok(append_result(&prepared[0], seqs[0])),
            BatchOutcome::FenceConflict { conflict } => Err(conflict.into_error().into()),
            BatchOutcome::Conflict { next } => Err(seq_conflict(
                stream,
                expected_seq.expect("only conditional appends conflict"),
                next,
                None,
            )
            .into()),
        }
    }

    fn validate_stream_batch(&self, members: &[StreamBatchMember]) -> RuntimeResult<()> {
        if members.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "stream.batch requires at least one member".into(),
            ));
        }
        let mut keys = HashSet::new();
        for member in members {
            match member {
                StreamBatchMember::Append(spec) => {
                    validate_stream(&spec.stream)?;
                    self.validate_note_kind(&spec.note_kind)?;
                    if let Some(fences) = &spec.fence {
                        fences.validate()?;
                        for fence in fences.entries() {
                            self.validate_note_kind(&fence.kind)?;
                        }
                    }
                    crate::secret_gate::check(
                        &serde_json::to_string(&spec.record)
                            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?,
                    )?;
                }
                StreamBatchMember::Write(spec) => {
                    self.validate_note_kind(&spec.kind)?;
                    NoteWriteOptions {
                        key: Some(spec.key.clone()),
                        expected_version: spec.expected_version,
                        embed: spec.embed,
                        fence: None,
                    }
                    .validate()?;
                    if spec.kind == "scheduled_event" {
                        return Err(RuntimeError::InvalidInput(
                            "scheduled_event notes are not writable through stream.batch; use schedule verbs".into(),
                        ));
                    }
                    if !keys.insert((&spec.kind, &spec.key)) {
                        return Err(RuntimeError::InvalidInput(format!(
                            "stream.batch repeats write key {:?} for kind {:?}",
                            spec.key, spec.kind,
                        )));
                    }
                    crate::secret_gate::check(
                        &serde_json::to_string(&spec.doc)
                            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?,
                    )?;
                    if let Some(tags) = &spec.tags {
                        crate::secret_gate::check_json(&json!({"tags": tags}))?;
                    }
                }
                StreamBatchMember::Refused(_) => {}
            }
        }
        Ok(())
    }

    async fn prepare_stream_write(
        &self,
        token: &NamespaceToken,
        spec: StreamWriteSpec,
        registry: &VerbRegistry,
    ) -> RuntimeResult<StreamBatchPreparation> {
        let content = serde_json::to_string(&spec.doc)
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        if let Some(expected) = spec.expected_version {
            let snapshot = match self
                .get_note_by_key(token, &spec.key, Some(&spec.kind), false)
                .await
            {
                Ok(note) => note,
                Err(RuntimeError::Khive(error))
                    if error.kind() == khive_types::ErrorKind::NotFound =>
                {
                    return Ok(StreamBatchPreparation::Ready(Box::new(
                        PreparedBatchAction::Refused(missing_write(&spec.key)),
                    )));
                }
                Err(error) => return Err(error),
            };
            let id = snapshot.id;
            snapshot
                .version
                .checked_add(1)
                .ok_or_else(|| RuntimeError::InvalidInput("note version exhausted".into()))?;
            let mut args = json!({
                "id": id, "kind": "note", "note_kind": spec.kind,
                "content": content, "expected_version": expected,
                "namespace": token.namespace().as_str(),
            });
            if let Some(tags) = spec.tags {
                args["properties"] = json!({"tags": tags});
            }
            if let Some(embed) = spec.embed {
                args["embed"] = json!(embed);
            }
            registry
                .prepare_note_update_hook(self, token, &snapshot, &mut args)
                .await?;
            let plan = crate::atomic_prepare::prepare_update_from_note_snapshot(
                self, token, &args, None, snapshot,
            )
            .await?;
            return Ok(StreamBatchPreparation::Ready(Box::new(
                PreparedBatchAction::Write {
                    key: spec.key,
                    kind: spec.kind,
                    id,
                    plan,
                    after_create: None,
                },
            )));
        }
        let mut args = json!({
            "kind": "note", "note_kind": spec.kind, "key": spec.key,
            "content": content, "namespace": token.namespace().as_str(),
        });
        if let Some(tags) = spec.tags {
            args["tags"] = json!(tags);
        }
        if let Some(embed) = spec.embed {
            args["embed"] = json!(embed);
        }
        if let Some(hook) = registry.find_kind_hook(&spec.kind) {
            hook.prepare_create(self, &mut args).await?;
        }
        let mut fields: StreamCreateFields = serde_json::from_value(args.clone())
            .map_err(|error| RuntimeError::InvalidInput(format!("stream write fields: {error}")))?;
        if let Some(tags) = fields.tags.take().filter(|tags| !tags.is_empty()) {
            let mut properties = match fields.properties.take() {
                None => serde_json::Map::new(),
                Some(Value::Object(properties)) => properties,
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(
                        "note tags require object properties".into(),
                    ))
                }
            };
            properties.insert("tags".into(), json!(tags));
            fields.properties = Some(Value::Object(properties));
        }
        let mut candidate = Note::new(token.namespace().as_str(), &spec.kind, &fields.content);
        candidate.name = fields.name.clone();
        candidate.properties = fields.properties.clone();
        crate::note_write::validate_head(&candidate)?;
        Ok(StreamBatchPreparation::Create(Box::new(
            PreparedStreamCreate {
                key: spec.key,
                kind: spec.kind,
                fields,
                args,
            },
        )))
    }

    async fn prepare_stream_batch(
        &self,
        token: &NamespaceToken,
        members: Vec<StreamBatchMember>,
        registry: &VerbRegistry,
    ) -> RuntimeResult<Vec<PreparedBatchMember>> {
        let mut pending = Vec::with_capacity(members.len());
        for member in members {
            let action = match member {
                StreamBatchMember::Refused(error) => {
                    StreamBatchPreparation::Ready(Box::new(PreparedBatchAction::Refused(error)))
                }
                StreamBatchMember::Write(spec) => {
                    match self.prepare_stream_write(token, spec, registry).await {
                        Ok(action) => action,
                        Err(RuntimeError::Khive(error))
                            if error.kind() == khive_types::ErrorKind::Conflict =>
                        {
                            StreamBatchPreparation::Ready(Box::new(PreparedBatchAction::Refused(
                                error,
                            )))
                        }
                        Err(error) => return Err(error),
                    }
                }
                StreamBatchMember::Append(spec) => {
                    let content = serde_json::to_string(&spec.record)
                        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                    StreamBatchPreparation::Append(Box::new((spec, content)))
                }
            };
            pending.push(action);
        }
        let requests = pending
            .iter()
            .filter_map(|action| match action {
                StreamBatchPreparation::Ready(_) => None,
                StreamBatchPreparation::Append(append) => {
                    let (spec, content) = append.as_ref();
                    Some(AtomicNoteRequest {
                        spec: AtomicNoteSpec {
                            token,
                            id: None,
                            kind: &spec.note_kind,
                            name: None,
                            content,
                            properties: spec.tags.clone().map(|tags| json!({"tags": tags})),
                        },
                        options: AtomicNoteOptions::default(),
                    })
                }
                StreamBatchPreparation::Create(create) => Some(AtomicNoteRequest {
                    spec: AtomicNoteSpec {
                        token,
                        id: None,
                        kind: &create.kind,
                        name: create.fields.name.as_deref(),
                        content: &create.fields.content,
                        properties: create.fields.properties.clone(),
                    },
                    options: AtomicNoteOptions {
                        salience: create.fields.salience,
                        key: Some(&create.key),
                        embed: Some(create.fields.embed.unwrap_or(create.kind != "head")),
                        ..Default::default()
                    },
                }),
            })
            .collect();
        let notes = prepare_atomic_note_requests(self, requests).await?;
        let mut note_plans = notes.notes.into_iter().zip(notes.plans);
        let mut prepared = Vec::with_capacity(pending.len());
        for (index, pending) in pending.into_iter().enumerate() {
            let action = match pending {
                StreamBatchPreparation::Ready(action) => *action,
                StreamBatchPreparation::Append(append) => {
                    let (spec, _) = *append;
                    let (note, plan) = note_plans.next().expect("one plan per new note");
                    PreparedBatchAction::Append {
                        stream: spec.stream,
                        expected_seq: spec.expected_seq,
                        note: Box::new(note),
                        plan,
                        fence: spec.fence,
                    }
                }
                StreamBatchPreparation::Create(create) => {
                    let (note, mut plan) = note_plans.next().expect("one plan per new note");
                    let AtomicOpPlan::AddNote(add) = &mut plan else {
                        return Err(RuntimeError::Internal(
                            "stream keyed create did not prepare a note".into(),
                        ));
                    };
                    add.post_commit = PostCommitEffect::NoteChanged {
                        note_id: note.id,
                        kind: note.kind.clone(),
                    };
                    PreparedBatchAction::Write {
                        key: create.key,
                        kind: create.kind,
                        id: note.id,
                        plan,
                        after_create: Some(create.args),
                    }
                }
            };
            prepared.push(PreparedBatchMember { index, action });
        }
        Ok(prepared)
    }

    async fn stream_batch_after_create(
        &self,
        registry: &VerbRegistry,
        hooks: Vec<(Uuid, String, Value)>,
    ) {
        for (id, kind, args) in hooks {
            if let Some(hook) = registry.find_kind_hook(&kind) {
                if let Err(error) = hook.after_create(self, id, &args).await {
                    tracing::warn!(%id, %kind, %error, "stream batch after_create failed after commit");
                }
            }
        }
    }

    /// All members share one transaction. Batch predicates precede member DML;
    /// effects become executable only after the complete unit commits.
    pub async fn stream_batch_atomic(
        &self,
        token: &NamespaceToken,
        members: Vec<StreamBatchMember>,
        fence: Option<NoteFence>,
        observed: Vec<StreamObservation>,
        registry: &VerbRegistry,
    ) -> RuntimeResult<Result<Vec<Value>, StreamBatchRefusal>> {
        self.validate_stream_batch(&members)?;
        if let Some(fence) = &fence {
            fence.validate()?;
            self.validate_note_kind(&fence.kind)?;
        }
        for entry in &observed {
            if entry.live_until.is_some() && entry.version.is_none() {
                return Err(RuntimeError::InvalidInput(
                    "observed live_until requires a positive version".into(),
                ));
            }
            crate::keyed_memory::validate_memory_key(&entry.key)?;
            self.validate_note_kind(&entry.kind)?;
            if entry.version.is_some_and(|version| version < 1) {
                return Err(RuntimeError::InvalidInput(
                    "observed version must be positive or null".into(),
                ));
            }
        }
        for (member, item) in members.iter().enumerate() {
            if let StreamBatchMember::Refused(error) = item {
                return Ok(Err(StreamBatchRefusal {
                    member,
                    error: place_member(error.clone(), Some(member))?,
                }));
            }
        }
        let prepared = self.prepare_stream_batch(token, members, registry).await?;
        let hooks = batch_create_hooks(&prepared);
        match run_prepared_stream_batch(
            self.sql().as_ref(),
            token.namespace().as_str().into(),
            prepared,
            fence,
            observed,
        )
        .await?
        {
            Ok((results, effects)) => {
                crate::atomic_prepare::apply_post_commit_effects_with_report(self, token, effects)
                    .await?;
                self.stream_batch_after_create(registry, hooks).await;
                Ok(Ok(results))
            }
            Err(refusal) => Ok(Err(refusal)),
        }
    }

    /// One writer transaction per member, in list order. Every member is
    /// prepared before the first write; a member's refusal is returned as its
    /// value and its siblings stand, so numbers on one stream increase with
    /// list position but another writer's append may fall between them.
    pub async fn stream_batch_per_member(
        &self,
        token: &NamespaceToken,
        members: Vec<StreamBatchMember>,
        registry: &VerbRegistry,
    ) -> RuntimeResult<Vec<Value>> {
        self.validate_stream_batch(&members)?;
        let mut results = Vec::with_capacity(members.len());
        let prepared = self.prepare_stream_batch(token, members, registry).await?;
        for member in prepared {
            if let PreparedBatchAction::Refused(error) = &member.action {
                results.push(refusal_value(&place_member(error.clone(), None)?)?);
                continue;
            }
            let hooks = batch_create_hooks(std::slice::from_ref(&member));
            match run_prepared_stream_batch(
                self.sql().as_ref(),
                token.namespace().as_str().into(),
                vec![member],
                None,
                vec![],
            )
            .await?
            {
                Ok((mut values, effects)) => {
                    crate::atomic_prepare::apply_post_commit_effects_with_report(
                        self, token, effects,
                    )
                    .await?;
                    self.stream_batch_after_create(registry, hooks).await;
                    results.append(&mut values);
                }
                Err(refusal) => results.push(refusal_value(&place_member(refusal.error, None)?)?),
            }
        }
        Ok(results)
    }

    /// Read one ordered page and its head from one SQL snapshot.
    pub async fn stream_read(
        &self,
        token: &NamespaceToken,
        stream: &str,
        after: i64,
        limit: i64,
    ) -> RuntimeResult<Value> {
        validate_stream(stream)?;
        if after < 0 || limit < 1 {
            return Err(RuntimeError::InvalidInput(
                "stream.read requires after >= 0 and limit >= 1".into(),
            ));
        }
        let mut reader = self.sql().reader().await?;
        let rows = reader.query_all(statement(
            "WITH head AS (SELECT COALESCE(MAX(seq),0) AS head_seq FROM note_streams WHERE namespace=?1 AND stream=?2), \
             page AS (SELECT s.seq,n.id,n.content,n.created_at FROM note_streams s JOIN notes n ON n.id=s.note_id \
                      WHERE s.namespace=?1 AND s.stream=?2 AND s.seq>?3 ORDER BY s.seq LIMIT ?4) \
             SELECT head.head_seq,page.seq,page.id,page.content,page.created_at FROM head LEFT JOIN page ON 1=1 ORDER BY page.seq",
            vec![SqlValue::Text(token.namespace().as_str().into()), SqlValue::Text(stream.into()), SqlValue::Integer(after), SqlValue::Integer(limit)],
        )).await?;
        let head = rows
            .first()
            .map(|row| integer(row, "head_seq"))
            .transpose()?
            .unwrap_or(0);
        let mut entries = Vec::new();
        let mut last = None;
        for row in &rows {
            if matches!(row.get("seq"), Some(SqlValue::Null)) {
                continue;
            }
            let seq = integer(row, "seq")?;
            let record: Value = serde_json::from_str(text(row, "content")?)
                .map_err(|e| RuntimeError::Internal(format!("invalid stream record JSON: {e}")))?;
            entries.push(json!({"seq": seq, "id": text(row, "id")?, "record": record, "created_at": micros_to_iso(integer(row, "created_at")?)}));
            last = Some(seq);
        }
        Ok(
            json!({"entries": entries, "head_seq": head, "next_after": last.filter(|last| *last < head)}),
        )
    }

    /// Independently count entries and read the head in the same statement.
    pub async fn stream_stat(&self, token: &NamespaceToken, stream: &str) -> RuntimeResult<Value> {
        validate_stream(stream)?;
        let row = self.sql().reader().await?.query_row(statement(
            "SELECT COUNT(*) AS count, COALESCE(MAX(seq),0) AS head_seq FROM note_streams WHERE namespace=?1 AND stream=?2",
            vec![SqlValue::Text(token.namespace().as_str().into()), SqlValue::Text(stream.into())],
        )).await?.ok_or_else(|| RuntimeError::Internal("stream.stat returned no aggregate row".into()))?;
        Ok(json!({"head_seq": integer(&row, "head_seq")?, "count": integer(&row, "count")?}))
    }

    /// Membership lookup only after the caller has resolved an accessible note.
    /// Match the stored namespace as well as its globally unique id.
    pub(crate) async fn stream_member_error(
        &self,
        note: &Note,
    ) -> RuntimeResult<Option<RuntimeError>> {
        let row = self
            .sql()
            .reader()
            .await?
            .query_row(statement(
                "SELECT stream,seq FROM note_streams WHERE namespace=?1 AND note_id=?2",
                vec![
                    SqlValue::Text(note.namespace.clone()),
                    SqlValue::Text(note.id.to_string()),
                ],
            ))
            .await?;
        row.map(|row| {
            Ok(KhiveError::conflict("stream entries are immutable")
                .with_details(Details::new_owned([
                    ("reason", "stream_member".into()),
                    ("id", note.id.to_string()),
                    ("stream", text(&row, "stream")?.into()),
                    ("seq", integer(&row, "seq")?.to_string()),
                ]))
                .into())
        })
        .transpose()
    }
}

#[cfg(test)]
#[path = "streams_batch_tests.rs"]
mod batch_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic_prepare::{prepare_delete, prepare_update};
    use crate::atomic_runner::{run_atomic_unit, AtomicRunOutcome};
    use crate::{Namespace, NotePatch};

    #[tokio::test]
    async fn stream_atomic_metadata_cas_preserves_record_and_refuses_stale_plan() {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let appended = rt
            .stream_append(
                &token,
                "cas",
                &json!({"n": 1}),
                None,
                "observation",
                None,
                None,
            )
            .await
            .unwrap();
        let id = uuid::Uuid::parse_str(appended["id"].as_str().unwrap()).unwrap();
        for args in [
            json!({"id": id, "content": "changed"}),
            json!({"id": id, "properties": {"x": 1}}),
        ] {
            let err = prepare_update(&rt, &token, &args, None).await.unwrap_err();
            assert!(matches!(err, RuntimeError::Khive(ref e) if e.details().is_some()));
            assert!(err.to_string().contains("immutable"));
        }
        for hard in [false, true] {
            let err = prepare_delete(&rt, &token, &json!({"id": id, "hard": hard}), None)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("immutable"));
        }
        let plan = prepare_update(&rt, &token, &json!({"id": id, "salience": 0.7}), None)
            .await
            .unwrap();
        assert!(matches!(
            run_atomic_unit(rt.sql().as_ref(), vec![plan])
                .await
                .unwrap(),
            AtomicRunOutcome::Committed { .. }
        ));
        let stale = prepare_update(&rt, &token, &json!({"id": id, "salience": 0.2}), None)
            .await
            .unwrap();
        rt.update_note(
            &token,
            id,
            NotePatch::new(None, None, Some(Some(0.9)), None, None),
        )
        .await
        .unwrap();
        assert!(matches!(
            run_atomic_unit(rt.sql().as_ref(), vec![stale])
                .await
                .unwrap(),
            AtomicRunOutcome::RolledBack { .. }
        ));
        let note = rt
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.salience, Some(0.9));
        assert_eq!(
            serde_json::from_str::<Value>(&note.content).unwrap(),
            json!({"n": 1})
        );
    }

    #[tokio::test]
    async fn stream_namespace_isolation_and_real_stat_count() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = rt.authorize(Namespace::parse("a").unwrap()).unwrap();
        let b = rt.authorize(Namespace::parse("b").unwrap()).unwrap();
        for token in [&a, &b] {
            assert_eq!(
                rt.stream_append(
                    token,
                    "same-name",
                    &json!(token.namespace().as_str()),
                    Some(1),
                    "observation",
                    None,
                    None
                )
                .await
                .unwrap()["seq"],
                1
            );
        }
        for token in [&a, &b] {
            let page = rt.stream_read(token, "same-name", 0, 10).await.unwrap();
            assert_eq!(page["entries"][0]["record"], token.namespace().as_str());
        }
        rt.sql().writer().await.unwrap().execute_script("DROP TRIGGER refuse_stream_ledger_update; UPDATE note_streams SET seq=5 WHERE namespace='a';".into()).await.unwrap();
        assert_eq!(
            rt.stream_stat(&a, "same-name").await.unwrap(),
            json!({"count": 1, "head_seq": 5})
        );
        assert_eq!(
            rt.stream_stat(&b, "same-name").await.unwrap(),
            json!({"count": 1, "head_seq": 1})
        );
    }
}
