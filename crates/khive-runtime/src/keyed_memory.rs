//! Memory identity is published only by the final DML of its atomic create.

use khive_storage::note::Note;
use khive_storage::{SqlStatement, SqlValue};
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

fn idempotency_conflict(key: &str, existing: &Note) -> RuntimeError {
    KhiveError::conflict(format!(
        "idempotency_key_conflict: key {key:?} already exists; stored content differs (existing memory {})",
        existing.id
    ))
        .with_details(Details::new_owned([
            ("reason", "idempotency_key_conflict".into()),
            ("key", key.to_owned()),
            ("existing_id", existing.id.to_string()),
        ]))
        .into()
}

async fn resolve_holder(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    key: &str,
) -> RuntimeResult<Option<Note>> {
    let mut matches = runtime
        .notes(token)?
        .get_live_notes_by_key(token.namespace().as_str(), key, Some("memory"))
        .await?;
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(RuntimeError::Internal(
            "memory key lookup returned multiple live holders".into(),
        )),
    }
}

fn missing_visibility_receipt(note_id: Uuid) -> RuntimeError {
    KhiveError::unavailable(format!(
        "freshness_unmet: original visibility receipt for memory {note_id} is unavailable"
    ))
    .with_details(Details::new_owned([
        ("reason", "freshness_unmet".into()),
        ("memory_id", note_id.to_string()),
    ]))
    .into()
}

/// Read the original per-model fences, including the explicit header for a
/// zero-model write. The join is one SQL statement so a concurrent hard delete
/// cannot pair a header from one snapshot with fences from another.
pub async fn memory_visibility_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    note_id: Uuid,
) -> RuntimeResult<Option<Vec<(String, u64)>>> {
    let mut reader = runtime.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT r.model_count, f.model, f.ann_write_log_seq \
                  FROM memory_visibility_receipts r \
                  LEFT JOIN memory_visibility_fences f \
                    ON f.namespace = r.namespace AND f.note_id = r.note_id \
                  WHERE r.namespace = ?1 AND r.note_id = ?2 ORDER BY f.model"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_owned()),
                SqlValue::Text(note_id.to_string()),
            ],
            label: Some("memory-visibility-receipt-read".into()),
        })
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let expected_model_count = match rows.first().and_then(|row| row.get("model_count")) {
        Some(SqlValue::Integer(count)) if *count >= 0 => *count as usize,
        _ => return Err(missing_visibility_receipt(note_id)),
    };
    let mut fences = Vec::new();
    for row in rows {
        match (row.get("model"), row.get("ann_write_log_seq")) {
            (Some(SqlValue::Text(model)), Some(SqlValue::Integer(seq)))
                if !model.is_empty() && *seq > 0 =>
            {
                fences.push((model.clone(), *seq as u64));
            }
            (Some(SqlValue::Null) | None, Some(SqlValue::Null) | None) => {}
            _ => return Err(missing_visibility_receipt(note_id)),
        }
    }
    if fences.len() != expected_model_count {
        return Err(missing_visibility_receipt(note_id));
    }
    Ok(Some(fences))
}

pub async fn create_keyed_memory(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    spec: KeyedMemorySpec<'_>,
) -> RuntimeResult<(Note, Option<Uuid>, bool)> {
    let (note, edge_id, replayed, _) =
        create_keyed_memory_with_report(runtime, token, spec).await?;
    Ok((note, edge_id, replayed))
}

/// Same as [`create_keyed_memory`], also returning the embedding-input
/// truncation report for a freshly written memory. A replay stores nothing
/// new, so it reports no truncation.
pub async fn create_keyed_memory_with_report(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    spec: KeyedMemorySpec<'_>,
) -> RuntimeResult<(
    Note,
    Option<Uuid>,
    bool,
    crate::retrieval::EmbeddingTruncationReport,
)> {
    let (note, edge_id, replayed, _, report) =
        create_keyed_memory_with_receipt_and_report(runtime, token, spec).await?;
    Ok((note, edge_id, replayed, report))
}

/// Receipt-bearing keyed create. The original per-model receipt is retained
/// on replay, including an explicit receipt for a write with zero models.
pub async fn create_keyed_memory_with_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    spec: KeyedMemorySpec<'_>,
) -> RuntimeResult<(Note, Option<Uuid>, bool, Vec<(String, u64)>)> {
    let (note, edge_id, replayed, fences, _) =
        create_keyed_memory_with_receipt_and_report(runtime, token, spec).await?;
    Ok((note, edge_id, replayed, fences))
}

/// Return both the original visibility receipt and the embedding-input report.
/// Replays retain the stored fences and report no newly truncated input.
pub async fn create_keyed_memory_with_receipt_and_report(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    spec: KeyedMemorySpec<'_>,
) -> RuntimeResult<(
    Note,
    Option<Uuid>,
    bool,
    Vec<(String, u64)>,
    crate::retrieval::EmbeddingTruncationReport,
)> {
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
            memory_visibility_receipt: true,
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
                let fences = memory_visibility_receipt(runtime, token, note.id)
                    .await?
                    .ok_or_else(|| missing_visibility_receipt(note.id))?;
                return Ok((note, edge_id, false, fences, prepared.embedding_truncation));
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
                    if holder.content == spec.content {
                        let fences = memory_visibility_receipt(runtime, token, holder.id)
                            .await?
                            .ok_or_else(|| missing_visibility_receipt(holder.id))?;
                        return Ok((
                            holder,
                            None,
                            true,
                            fences,
                            crate::retrieval::EmbeddingTruncationReport::default(),
                        ));
                    }
                    return Err(idempotency_conflict(spec.key, &holder));
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
