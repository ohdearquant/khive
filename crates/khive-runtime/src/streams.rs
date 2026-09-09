//! Ordered note streams. Embedding preparation precedes one SQL-only writer
//! transaction containing the sequence predicate, note/index writes and ledger.
use std::any::Any;

use khive_storage::{
    AtomicUnitOp, Note, SqlRow, SqlStatement, SqlValue, StorageCapability, StorageError,
};
use khive_types::{Details, KhiveError};
use serde_json::{json, Value};

use crate::atomic_message::{prepare_atomic_notes, AtomicNoteOptions, AtomicNoteSpec};
use crate::atomic_runner::AtomicOpPlan;
use crate::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

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

enum AppendOutcome {
    Appended(i64),
    Conflict(i64),
}

impl KhiveRuntime {
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
        validate_stream(stream)?;
        let content =
            serde_json::to_string(record).map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
        let mut prepared = prepare_atomic_notes(
            self,
            vec![AtomicNoteSpec {
                token,
                id: None,
                kind: note_kind,
                name: None,
                content: &content,
                properties: tags.map(|tags| json!({"tags": tags})),
            }],
            AtomicNoteOptions::default(),
        )
        .await?;
        let note = prepared.notes.remove(0);
        let AtomicOpPlan::AddNote(plan) = prepared.plans.remove(0) else {
            return Err(RuntimeError::Internal(
                "stream preparation did not produce a note".into(),
            ));
        };
        let ns = token.namespace().as_str().to_string();
        let stream_owned = stream.to_string();
        let note_id = note.id.to_string();
        // All allocations for embedding, note normalization and SQL plans have
        // completed. Only bounded statement driving occurs while the writer is held.
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let scope = vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(stream_owned.clone()),
                ];
                let head = writer.query_scalar(statement(
                "SELECT COALESCE(MAX(seq), 0) FROM note_streams WHERE namespace=?1 AND stream=?2", scope,
            )).await?;
                let Some(SqlValue::Integer(head)) = head else {
                    return Err(write_failure("invalid stream head"));
                };
                let next = head
                    .checked_add(1)
                    .ok_or_else(|| write_failure("stream sequence exhausted"))?;
                if expected_seq.is_some_and(|expected| expected != next) {
                    return Ok(Box::new(AppendOutcome::Conflict(next)) as Box<dyn Any + Send>);
                }
                for planned in plan.statements {
                    let affected = writer.execute(planned.statement).await?;
                    if planned
                        .guard
                        .is_some_and(|guard| !guard.holds_for(affected))
                    {
                        return Err(write_failure("prepared note write guard failed"));
                    }
                }
                writer.execute(statement(
                "INSERT INTO note_streams(namespace,stream,seq,note_id) VALUES (?1,?2,?3,?4)",
                vec![SqlValue::Text(ns), SqlValue::Text(stream_owned), SqlValue::Integer(next), SqlValue::Text(note_id)],
            )).await?;
                Ok(Box::new(AppendOutcome::Appended(next)) as Box<dyn Any + Send>)
            })
        });
        let outcome = self
            .sql()
            .atomic_unit(op)
            .await?
            .downcast::<AppendOutcome>()
            .map_err(|_| RuntimeError::Internal("invalid stream append outcome".into()))?;
        match *outcome {
            AppendOutcome::Appended(seq) => {
                Ok(json!({"seq": seq, "id": note.id, "created_at": micros_to_iso(note.created_at)}))
            }
            AppendOutcome::Conflict(next) => {
                Err(KhiveError::conflict("stream sequence precondition failed")
                    .with_details(Details::new_owned([
                        ("reason", "seq_conflict".into()),
                        ("stream", stream.into()),
                        (
                            "expected_seq",
                            expected_seq
                                .expect("only conditional appends conflict")
                                .to_string(),
                        ),
                        ("next_seq", next.to_string()),
                    ]))
                    .into())
            }
        }
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
