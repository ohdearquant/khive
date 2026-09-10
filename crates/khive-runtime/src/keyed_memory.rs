//! Memory identity is published only by the final DML of its atomic create.

use khive_storage::note::Note;
use khive_storage::types::SqlValue;
use khive_storage::SqlStatement;
use khive_types::{Details, KhiveError};
use serde_json::Value;
use uuid::Uuid;

use crate::atomic_message::{AtomicNoteOptions, AtomicNoteSpec};
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicRunOutcome};
use crate::note_create::{prepare_note_create, KeyPublication, KEY_CLAIM};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

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
    let (mut prepared, annotation_ids) = prepare_note_create(
        runtime,
        AtomicNoteSpec {
            token,
            id: None,
            kind: "memory",
            name: None,
            content: spec.content,
            properties: Some(spec.properties),
        },
        AtomicNoteOptions {
            salience: Some(spec.salience),
            decay_factor: Some(spec.decay_factor),
            embedding_model: spec.embedding_model,
            key: Some(spec.key),
            ..Default::default()
        },
        &spec.source_id.into_iter().collect::<Vec<_>>(),
        KeyPublication::AfterDependents,
    )
    .await?;
    let mut note = prepared.notes.remove(0);
    let edge_id = annotation_ids.first().copied();

    for _attempt in 0..2 {
        #[cfg(test)]
        crate::keyed_memory_tests::checkpoint(token.namespace().as_str(), _attempt, false).await;
        match run_atomic_unit(runtime.sql().as_ref(), prepared.plans.clone()).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                note.key = Some(spec.key.to_owned());
                note.version = 2;
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
