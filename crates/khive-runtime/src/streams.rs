//! Ordered note streams. Embedding preparation precedes one SQL-only writer
//! transaction containing the sequence predicates, note/index writes and ledger
//! rows. A batch is that transaction over several members (atomic mode) or one
//! such transaction per member, in list order (per-member mode).
use std::any::Any;

use khive_storage::{
    AtomicUnitOp, Note, SqlRow, SqlStatement, SqlValue, StorageCapability, StorageError,
};
use khive_types::{Details, KhiveError};
use serde_json::{json, Value};

use crate::atomic_message::{prepare_atomic_notes, AtomicNoteOptions, AtomicNoteSpec};
use crate::atomic_plan::PlanStatement;
use crate::atomic_runner::AtomicOpPlan;
use crate::{
    micros_to_iso, DomainDisposition, KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult,
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
}

/// A batch member as the verb layer resolved it: an append to run, or the
/// refusal the member already earned (an op naming no member operation, a
/// member kind this server does not carry). The mode places the refusal.
pub enum StreamBatchMember {
    Append(StreamAppendSpec),
    Refused(KhiveError),
}

/// The member refusal that stopped an atomic batch; nothing was written.
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
}

fn append_result(prepared: &PreparedAppend, seq: i64) -> Value {
    json!({"seq": seq, "id": prepared.note.id, "created_at": micros_to_iso(prepared.note.created_at)})
}

enum BatchOutcome {
    Appended(Vec<i64>),
    Conflict { member: usize, next: i64 },
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
            out.push(PreparedAppend {
                stream: spec.stream.clone(),
                expected_seq: spec.expected_seq,
                note,
                statements: plan.statements,
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
        let entries: Vec<(String, Option<i64>, String, Vec<PlanStatement>)> = appends
            .iter()
            .map(|a| {
                (
                    a.stream.clone(),
                    a.expected_seq,
                    a.note.id.to_string(),
                    a.statements.clone(),
                )
            })
            .collect();
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let mut heads: Vec<(String, i64)> = Vec::new();
                for (stream, _, _, _) in &entries {
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
                for (member, (stream, expected_seq, _, _)) in entries.iter().enumerate() {
                    let head = heads
                        .iter_mut()
                        .find(|(known, _)| known == stream)
                        .map(|(_, head)| head)
                        .ok_or_else(|| write_failure("stream head missing"))?;
                    let next = head
                        .checked_add(1)
                        .ok_or_else(|| write_failure("stream sequence exhausted"))?;
                    if expected_seq.is_some_and(|expected| expected != next) {
                        return Ok(Box::new(BatchOutcome::Conflict { member, next })
                            as Box<dyn Any + Send>);
                    }
                    *head = next;
                    assigned.push(next);
                }
                for ((stream, _, note_id, statements), seq) in
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
                    writer
                        .execute(statement(
                            "INSERT INTO note_streams(namespace,stream,seq,note_id) VALUES (?1,?2,?3,?4)",
                            vec![
                                SqlValue::Text(ns.clone()),
                                SqlValue::Text(stream),
                                SqlValue::Integer(seq),
                                SqlValue::Text(note_id),
                            ],
                        ))
                        .await?;
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
    pub async fn stream_append(
        &self,
        token: &NamespaceToken,
        stream: &str,
        record: &Value,
        expected_seq: Option<i64>,
        note_kind: &str,
        tags: Option<Vec<String>>,
    ) -> RuntimeResult<Value> {
        let spec = StreamAppendSpec {
            stream: stream.to_string(),
            record: record.clone(),
            expected_seq,
            note_kind: note_kind.to_string(),
            tags,
        };
        let prepared = self.prepare_stream_appends(token, &[&spec]).await?;
        match self.run_stream_appends(token, &prepared).await? {
            BatchOutcome::Appended(seqs) => Ok(append_result(&prepared[0], seqs[0])),
            BatchOutcome::Conflict { next, .. } => Err(seq_conflict(
                stream,
                expected_seq.expect("only conditional appends conflict"),
                next,
                None,
            )
            .into()),
        }
    }

    /// One writer transaction over every member. Shape and content are
    /// validated for every member before anything is written; a refused member
    /// (its own refusal or a sequence conflict) refuses the whole batch with
    /// nothing written, and success means every member committed.
    pub async fn stream_batch_atomic(
        &self,
        token: &NamespaceToken,
        members: Vec<StreamBatchMember>,
    ) -> RuntimeResult<Result<Vec<Value>, StreamBatchRefusal>> {
        let specs: Vec<&StreamAppendSpec> = members
            .iter()
            .filter_map(|member| match member {
                StreamBatchMember::Append(spec) => Some(spec),
                StreamBatchMember::Refused(_) => None,
            })
            .collect();
        let prepared = self.prepare_stream_appends(token, &specs).await?;
        for (member, item) in members.iter().enumerate() {
            if let StreamBatchMember::Refused(error) = item {
                return Ok(Err(StreamBatchRefusal {
                    member,
                    error: error.clone(),
                }));
            }
        }
        match self.run_stream_appends(token, &prepared).await? {
            BatchOutcome::Appended(seqs) => Ok(Ok(prepared
                .iter()
                .zip(seqs)
                .map(|(append, seq)| append_result(append, seq))
                .collect())),
            BatchOutcome::Conflict { member, next } => Ok(Err(StreamBatchRefusal {
                member,
                error: seq_conflict(
                    &prepared[member].stream,
                    prepared[member]
                        .expected_seq
                        .expect("only conditional appends conflict"),
                    next,
                    Some(member),
                ),
            })),
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
    ) -> RuntimeResult<Vec<Value>> {
        let specs: Vec<&StreamAppendSpec> = members
            .iter()
            .filter_map(|member| match member {
                StreamBatchMember::Append(spec) => Some(spec),
                StreamBatchMember::Refused(_) => None,
            })
            .collect();
        let mut prepared = self
            .prepare_stream_appends(token, &specs)
            .await?
            .into_iter();
        let mut results = Vec::with_capacity(members.len());
        for member in &members {
            match member {
                StreamBatchMember::Refused(error) => results.push(refusal_value(error)?),
                StreamBatchMember::Append(_) => {
                    let append = prepared
                        .next()
                        .ok_or_else(|| RuntimeError::Internal("prepared append missing".into()))?;
                    match self
                        .run_stream_appends(token, std::slice::from_ref(&append))
                        .await?
                    {
                        BatchOutcome::Appended(seqs) => {
                            results.push(append_result(&append, seqs[0]))
                        }
                        BatchOutcome::Conflict { next, .. } => {
                            results.push(refusal_value(&seq_conflict(
                                &append.stream,
                                append
                                    .expected_seq
                                    .expect("only conditional appends conflict"),
                                next,
                                None,
                            ))?)
                        }
                    }
                }
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
            .stream_append(&token, "cas", &json!({"n": 1}), None, "observation", None)
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
