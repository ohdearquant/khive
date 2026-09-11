//! A message pair publishes its outbound identity in the final atomic statement.

use khive_storage::{Note, SqlStatement, SqlValue};
use khive_types::{Details, KhiveError};
use uuid::Uuid;

use crate::atomic_message::{prepare_atomic_notes, AtomicNoteOptions, AtomicNoteSpec};
use crate::atomic_plan::{AffectedRowGuard, PlanStatement};
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use crate::{KhiveRuntime, RuntimeError, RuntimeResult};

const KEY_CLAIM: &str = "message-key-claim";

pub enum KeyedMessageWrite {
    Created {
        notes: Vec<Note>,
        embedding_truncation: crate::retrieval::EmbeddingTruncationReport,
    },
    Existing(Uuid),
}

/// Create exactly two caller-namespaced message notes or return the outbound
/// holder after rolling back a competing pair. The comm layer verifies payload
/// and sibling integrity before treating `Existing` as a successful replay.
pub async fn create_keyed_message_pair(
    runtime: &KhiveRuntime,
    specs: [AtomicNoteSpec<'_>; 2],
    physical_key: &str,
) -> RuntimeResult<KeyedMessageWrite> {
    let namespace = specs[0].token.namespace().as_str().to_owned();
    if specs.iter().any(|spec| spec.kind != "message")
        || specs[1].token.namespace().as_str() != namespace
        || specs[0].token.actor().id != specs[1].token.actor().id
        || specs[0].id.is_none()
        || specs[1].id.is_none()
        || specs[0].id == specs[1].id
    {
        return Err(RuntimeError::InvalidInput(
            "a keyed message pair requires distinct IDs in one actor namespace".into(),
        ));
    }
    let mut prepared =
        prepare_atomic_notes(runtime, specs.into(), AtomicNoteOptions::default()).await?;
    let outbound_id = prepared.notes[0].id;
    for (plan, note) in prepared.plans.iter_mut().zip(&prepared.notes) {
        let AtomicOpPlan::AddNote(plan) = plan else {
            return Err(RuntimeError::Internal(
                "expected prepared message note".into(),
            ));
        };
        // A caller-supplied UUID must never turn pair creation into an upsert.
        plan.statements[0].statement =
            khive_db::stores::note::note_insert_if_absent_statement(note);
    }
    let Some(AtomicOpPlan::AddNote(last)) = prepared.plans.last_mut() else {
        return Err(RuntimeError::Internal(
            "expected recipient message plan".into(),
        ));
    };
    last.statements.push(PlanStatement {
        statement: SqlStatement {
            sql: "UPDATE OR IGNORE notes SET key = ?1 WHERE id = ?2 AND namespace = ?3 \
                  AND kind = 'message' AND key IS NULL AND deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Text(physical_key.to_owned()),
                SqlValue::Text(outbound_id.to_string()),
                SqlValue::Text(namespace.clone()),
            ],
            label: Some(KEY_CLAIM.into()),
        },
        guard: Some(AffectedRowGuard::exactly(1)),
    });

    match run_atomic_unit(runtime.sql().as_ref(), prepared.plans).await {
        Ok(AtomicRunOutcome::Committed { .. }) => {
            prepared.notes[0].key = Some(physical_key.to_owned());
            Ok(KeyedMessageWrite::Created {
                notes: prepared.notes,
                embedding_truncation: prepared.embedding_truncation,
            })
        }
        Ok(AtomicRunOutcome::RolledBack {
            failure:
                AtomicOpFailure::GuardFailed {
                    statement_label,
                    observed: 0,
                    ..
                },
            ..
        }) if statement_label.as_deref() == Some(KEY_CLAIM) => {
            let holder = runtime
                .sql()
                .reader()
                .await?
                .query_scalar(SqlStatement {
                    sql: "SELECT id FROM notes WHERE namespace = ?1 AND kind = 'message' \
                      AND key = ?2 AND deleted_at IS NULL LIMIT 1"
                        .into(),
                    params: vec![
                        SqlValue::Text(namespace),
                        SqlValue::Text(physical_key.to_owned()),
                    ],
                    label: Some("message-key-holder".into()),
                })
                .await?;
            match holder {
                Some(SqlValue::Text(id)) => Uuid::parse_str(&id)
                    .map(KeyedMessageWrite::Existing)
                    .map_err(|error| {
                        RuntimeError::Internal(format!("invalid message holder id: {error}"))
                    }),
                None => Err(KhiveError::unavailable(
                    "message key holder disappeared during reconciliation",
                )
                .with_details(Details::new_owned([(
                    "reason",
                    "key_holder_unresolved".into(),
                )]))
                .into()),
                Some(_) => Err(RuntimeError::Internal(
                    "message holder id is not text".into(),
                )),
            }
        }
        Ok(AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        }) => Err(RuntimeError::Internal(format!(
            "atomic message pair rolled back at op {failed_op_index}: {failure:?}"
        ))),
        Err(error) => Err(RuntimeError::Storage(error.0)),
    }
}
