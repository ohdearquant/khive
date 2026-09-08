//! Memory identity is published only by the final DML of its atomic create.

use khive_storage::note::Note;
use khive_storage::types::{Edge, LinkId, SqlValue};
use khive_storage::{EdgeRelation, SqlStatement};
use khive_types::{Details, KhiveError};
use serde_json::Value;
use uuid::Uuid;

use crate::atomic_message::{prepare_atomic_notes, AtomicNoteOptions, AtomicNoteSpec};
use crate::atomic_plan::{AffectedRowGuard, PlanStatement};
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

const KEY_CLAIM: &str = "memory-key-claim";

pub struct KeyedMemorySpec<'a> {
    pub content: &'a str,
    pub key: &'a str,
    pub salience: f64,
    pub decay_factor: f64,
    pub properties: Value,
    pub source_id: Option<Uuid>,
    pub embedding_model: Option<&'a str>,
}

pub fn validate_memory_key(key: &str) -> RuntimeResult<()> {
    if key.len() > 512 || key.contains('\0') {
        return Err(RuntimeError::InvalidInput(
            "key must be at most 512 UTF-8 bytes and must not contain U+0000".into(),
        ));
    }
    Ok(())
}

fn key_conflict(key: &str, existing_id: Uuid) -> RuntimeError {
    KhiveError::conflict("a live memory already holds this key")
        .with_details(Details::new_owned([
            ("reason", "key_conflict".into()),
            ("key", key.to_owned()),
            ("existing_id", existing_id.to_string()),
        ]))
        .into()
}

async fn resolve_holder(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    key: &str,
) -> RuntimeResult<Option<Uuid>> {
    let value = runtime
        .sql()
        .reader()
        .await?
        .query_scalar(SqlStatement {
            sql: "SELECT id FROM notes WHERE namespace = ?1 AND kind = 'memory' \
                  AND key = ?2 AND deleted_at IS NULL LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_owned()),
                SqlValue::Text(key.to_owned()),
            ],
            label: Some("memory-key-holder".into()),
        })
        .await?;
    match value {
        None => Ok(None),
        Some(SqlValue::Text(id)) => Uuid::parse_str(&id)
            .map(Some)
            .map_err(|error| RuntimeError::Internal(format!("invalid memory holder id: {error}"))),
        Some(_) => Err(RuntimeError::Internal(
            "memory holder id is not text".into(),
        )),
    }
}

pub async fn create_keyed_memory(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    spec: KeyedMemorySpec<'_>,
) -> RuntimeResult<(Note, Option<Uuid>)> {
    validate_memory_key(spec.key)?;
    if spec.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "content must not be empty".into(),
        ));
    }
    if let Some(target) = spec.source_id {
        if !runtime.substrate_exists_by_id(token, target).await? {
            return Err(RuntimeError::NotFound(format!(
                "create_note annotates target {target} not found"
            )));
        }
    }
    let mut prepared = prepare_atomic_notes(
        runtime,
        vec![AtomicNoteSpec {
            token,
            id: None,
            kind: "memory",
            name: None,
            content: spec.content,
            properties: Some(spec.properties),
        }],
        AtomicNoteOptions {
            salience: Some(spec.salience),
            decay_factor: Some(spec.decay_factor),
            embedding_model: spec.embedding_model,
        },
    )
    .await?;
    let mut note = prepared.notes.remove(0);
    let AtomicOpPlan::AddNote(plan) = &mut prepared.plans[0] else {
        return Err(RuntimeError::Internal(
            "expected prepared memory note".into(),
        ));
    };
    plan.statements[0].statement = khive_db::stores::note::note_insert_if_absent_statement(&note);
    let edge_id = spec.source_id.map(|target_id| {
        let now = chrono::Utc::now();
        let id = Uuid::new_v4();
        let edge = Edge {
            id: LinkId::from(id),
            namespace: note.namespace.clone(),
            source_id: note.id,
            target_id,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        };
        plan.statements.push(PlanStatement {
            statement: khive_db::stores::graph::edge_insert_only_guarded_by_endpoints_statement(
                &edge,
            ),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
        id
    });
    // A competing claim rolls back all provisional rows, including FTS and annotations.
    plan.statements.push(PlanStatement {
        statement: SqlStatement {
            sql: "UPDATE OR IGNORE notes SET key = ?1 WHERE id = ?2 AND namespace = ?3 \
                  AND kind = 'memory' AND key IS NULL AND deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Text(spec.key.to_owned()),
                SqlValue::Text(note.id.to_string()),
                SqlValue::Text(note.namespace.clone()),
            ],
            label: Some(KEY_CLAIM.into()),
        },
        guard: Some(AffectedRowGuard::exactly(1)),
    });

    for _attempt in 0..2 {
        #[cfg(test)]
        crate::keyed_memory_tests::checkpoint(token.namespace().as_str(), _attempt, false).await;
        match run_atomic_unit(runtime.sql().as_ref(), prepared.plans.clone()).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                note.key = Some(spec.key.to_owned());
                return Ok((note, edge_id));
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
                #[cfg(test)]
                crate::keyed_memory_tests::checkpoint(token.namespace().as_str(), _attempt, true)
                    .await;
                if let Some(holder) = resolve_holder(runtime, token, spec.key).await? {
                    return Err(key_conflict(spec.key, holder));
                }
            }
            Ok(AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            }) => {
                return Err(RuntimeError::Internal(format!(
                    "atomic memory write rolled back at op {failed_op_index}: {failure:?}"
                )));
            }
            Err(error) => return Err(RuntimeError::Storage(error.0)),
        }
    }
    Err(
        KhiveError::unavailable("memory key holder disappeared during reconciliation")
            .with_details(Details::new_owned([
                ("reason", "key_holder_unresolved".into()),
                ("key", spec.key.to_owned()),
            ]))
            .into(),
    )
}
