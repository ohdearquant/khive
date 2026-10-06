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

struct VisibilityReceiptState {
    note_present: bool,
    epoch: Option<String>,
    receipt_present: bool,
    fences: Option<Vec<(String, u64)>>,
}

/// Join identity, independent provenance and original receipt in one snapshot.
/// Joins use note identity first so a conflicting namespace cannot disappear
/// behind a filter and masquerade as a genuinely absent receipt or marker.
async fn visibility_receipt_state(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    note_id: Uuid,
) -> RuntimeResult<VisibilityReceiptState> {
    let unavailable = || {
        crate::visibility_receipts::receipt_failure(
            "receipt_store_unavailable",
            Some(note_id),
            false,
        )
    };
    runtime
        .require_visibility_cutover()
        .map_err(|_| unavailable())?;
    let mut reader = runtime.sql().reader().await.map_err(|_| unavailable())?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT n.kind AS note_kind, n.namespace AS note_namespace, \
              e.namespace AS epoch_namespace, e.epoch, \
              r.note_id AS receipt_id, r.namespace AS receipt_namespace, r.model_count, \
              f.namespace AS fence_namespace, f.model, f.ann_write_log_seq \
              FROM notes n LEFT JOIN memory_visibility_epochs e ON e.note_id = n.id \
              LEFT JOIN memory_visibility_receipts r ON r.note_id = n.id \
              LEFT JOIN memory_visibility_fences f ON f.note_id = n.id \
              WHERE n.id = ?1 ORDER BY r.namespace, f.namespace, f.model"
                .into(),
            params: vec![SqlValue::Text(note_id.to_string())],
            label: Some("memory-visibility-receipt-read".into()),
        })
        .await
        .map_err(|_| unavailable())?;
    let mut state = VisibilityReceiptState {
        note_present: !rows.is_empty(),
        epoch: None,
        receipt_present: false,
        fences: None,
    };
    let Some(first) = rows.first() else {
        return Ok(state);
    };
    let namespace = token.namespace().as_str();
    let text = |value: Option<&SqlValue>, expected: &str| match value {
        Some(SqlValue::Text(actual)) => actual == expected,
        _ => false,
    };
    let identity_matches = text(first.get("note_kind"), "memory")
        && text(first.get("note_namespace"), namespace)
        && text(first.get("epoch_namespace"), namespace);
    if identity_matches {
        if let Some(SqlValue::Text(epoch)) = first.get("epoch") {
            state.epoch = Some(epoch.clone());
        }
    }
    state.receipt_present = rows
        .iter()
        .any(|row| matches!(row.get("receipt_id"), Some(SqlValue::Text(_))));
    let expected_count = match first.get("model_count") {
        Some(SqlValue::Integer(count)) if *count >= 0 => usize::try_from(*count).ok(),
        _ => None,
    };
    let mut valid = text(first.get("note_kind"), "memory")
        && text(first.get("note_namespace"), namespace)
        && state.receipt_present
        && expected_count.is_some();
    let mut fences = std::collections::BTreeMap::new();
    for row in &rows {
        if (matches!(row.get("receipt_id"), Some(SqlValue::Text(_)))
            && !text(row.get("receipt_namespace"), namespace))
            || matches!(row.get("fence_namespace"), Some(SqlValue::Text(ns))
                if ns != namespace || !text(row.get("receipt_namespace"), ns))
        {
            state.epoch = None;
        }
        valid &= text(row.get("receipt_namespace"), namespace)
            && matches!(row.get("model_count"), Some(SqlValue::Integer(count))
                if usize::try_from(*count).ok() == expected_count);
        match (
            row.get("fence_namespace"),
            row.get("model"),
            row.get("ann_write_log_seq"),
        ) {
            (
                Some(SqlValue::Text(ns)),
                Some(SqlValue::Text(model)),
                Some(SqlValue::Integer(seq)),
            ) if ns == namespace && !model.is_empty() && *seq > 0 => {
                valid &= fences.insert(model.clone(), *seq as u64).is_none();
            }
            (
                Some(SqlValue::Null) | None,
                Some(SqlValue::Null) | None,
                Some(SqlValue::Null) | None,
            ) => {}
            _ => valid = false,
        }
    }
    if valid && expected_count == Some(fences.len()) {
        state.fences = Some(fences.into_iter().collect());
    }
    Ok(state)
}

/// Read original fences without issuing a token. Callers needing a replay token
/// must additionally apply the independent provenance classification below.
pub async fn memory_visibility_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    note_id: Uuid,
) -> RuntimeResult<Option<Vec<(String, u64)>>> {
    Ok(visibility_receipt_state(runtime, token, note_id)
        .await?
        .fences)
}

async fn classified_visibility_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    note_id: Uuid,
    replay: bool,
) -> RuntimeResult<Vec<(String, u64)>> {
    let state = visibility_receipt_state(runtime, token, note_id).await?;
    if replay && !state.note_present {
        return Err(RuntimeError::NotFound(
            "memory not found during keyed replay".into(),
        ));
    }
    let reason = match state.epoch.as_deref() {
        Some("modern") => {
            if let Some(fences) = state.fences {
                return Ok(fences);
            }
            "receipt_temporarily_unavailable"
        }
        Some("legacy") if !state.receipt_present => "legacy_receipt_absent",
        _ => "receipt_epoch_unknown",
    };
    Err(crate::visibility_receipts::receipt_failure(
        reason,
        Some(note_id),
        replay,
    ))
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
/// truncation report computed for this call. A replay stores nothing new but
/// retains the report from preparing this call's identical content.
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
/// Replays retain the stored fences and the report computed while preparing
/// this call's embedding input; the report does not describe the original write.
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
                let fences = classified_visibility_receipt(runtime, token, note.id, false).await?;
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
                        let fences =
                            classified_visibility_receipt(runtime, token, holder.id, true).await?;
                        return Ok((holder, None, true, fences, prepared.embedding_truncation));
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

#[cfg(test)]
mod receipt_read_tests {
    use super::*;
    use crate::DomainDisposition;

    const KEY: &str = "receipt-race-key";
    const CONTENT: &str = "private receipt race content";

    async fn captured_holder() -> (KhiveRuntime, NamespaceToken, Note) {
        let runtime = KhiveRuntime::memory().unwrap();
        runtime.install_kind_registry(vec![], vec!["memory".into()]);
        let token = runtime
            .authorize(khive_types::Namespace::parse("receipt-read-race").unwrap())
            .unwrap();
        let (note, _, replayed, fences) = create_keyed_memory_with_receipt(
            &runtime,
            &token,
            KeyedMemorySpec {
                content: CONTENT,
                key: KEY,
                salience: 0.7,
                decay_factor: 0.95,
                properties: serde_json::json!({}),
                source_id: None,
                embedding_model: None,
            },
        )
        .await
        .unwrap();
        assert!(!replayed);
        assert!(fences.is_empty());
        let holder = resolve_holder(&runtime, &token, KEY)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(holder, note);
        assert!(
            classified_visibility_receipt(&runtime, &token, holder.id, true)
                .await
                .unwrap()
                .is_empty()
        );
        (runtime, token, holder)
    }

    #[tokio::test]
    async fn replay_receipt_reports_missing_after_captured_holder_is_hard_deleted() {
        let (runtime, token, holder) = captured_holder().await;
        // This is the exact boundary in the replay path: holder lookup has
        // completed, but its joined receipt/epoch read has not started.
        assert!(runtime.delete_note(&token, holder.id, true).await.unwrap());
        assert!(runtime
            .notes(&token)
            .unwrap()
            .get_note_including_deleted(holder.id)
            .await
            .unwrap()
            .is_none());
        let error = classified_visibility_receipt(&runtime, &token, holder.id, true)
            .await
            .unwrap_err();
        assert!(matches!(&error, RuntimeError::NotFound(_)));
        let value = crate::error_projection::runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(value["kind"], "not_found");
        assert_eq!(value["details"], Value::Null);
        assert_eq!(value["domain_disposition"], "unknown");
        assert!(value.get("retryable").is_none());
        let encoded = value.to_string();
        let id = holder.id.to_string();
        for private in [KEY, CONTENT, id.as_str()] {
            assert!(!encoded.contains(private));
        }
        assert!(memory_visibility_receipt(&runtime, &token, holder.id)
            .await
            .unwrap()
            .is_none());
        assert!(resolve_holder(&runtime, &token, KEY)
            .await
            .unwrap()
            .is_none());

        // The post-commit caller retains its existing uncertainty semantics.
        let error = classified_visibility_receipt(&runtime, &token, holder.id, false)
            .await
            .unwrap_err();
        let value = crate::error_projection::runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(value["details"]["reason"], "receipt_epoch_unknown");
        assert_eq!(value["domain_disposition"], "unknown");
    }

    #[tokio::test]
    async fn present_holder_with_missing_or_invalid_epoch_is_not_missing() {
        for mutation in [
            "DELETE FROM memory_visibility_epochs WHERE note_id = ?1",
            "UPDATE memory_visibility_epochs SET epoch = 'unknown' WHERE note_id = ?1",
            "UPDATE memory_visibility_epochs SET epoch = 'malformed' WHERE note_id = ?1",
            "UPDATE memory_visibility_epochs SET epoch = 'legacy' WHERE note_id = ?1",
            "UPDATE memory_visibility_epochs SET namespace = 'foreign-private-namespace' WHERE note_id = ?1",
            "UPDATE notes SET namespace = 'foreign-private-namespace' WHERE id = ?1",
        ] {
            let (runtime, token, holder) = captured_holder().await;
            {
                // Model a damaged epoch value as well as valid but incomplete
                // provenance. Restore constraint checks before reading it.
                let writer = runtime.backend().pool().try_writer().unwrap();
                writer
                    .conn()
                    .pragma_update(None, "ignore_check_constraints", true)
                    .unwrap();
                writer.conn().execute(mutation, [holder.id.to_string()]).unwrap();
                writer
                    .conn()
                    .pragma_update(None, "ignore_check_constraints", false)
                    .unwrap();
            }
            assert!(runtime
                .notes(&token)
                .unwrap()
                .get_note_including_deleted(holder.id)
                .await
                .unwrap()
                .is_some());
            let error = classified_visibility_receipt(&runtime, &token, holder.id, true)
                .await
                .unwrap_err();
            let value = crate::error_projection::runtime_error_value(error, DomainDisposition::Unknown);
            assert_eq!(value["details"]["reason"], "receipt_epoch_unknown");
            assert_eq!(value["details"]["memory_id"], holder.id.to_string());
            assert_eq!(value["domain_disposition"], "not_committed");
            assert_eq!(value["retryable"], false);
            for private in [KEY, CONTENT, "foreign-private-namespace"] {
                assert!(!value.to_string().contains(private));
            }
        }
    }

    #[tokio::test]
    async fn unreadable_note_store_is_unavailable_not_missing() {
        let (runtime, token, holder) = captured_holder().await;
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: "ALTER TABLE notes RENAME TO private_unreadable_notes".into(),
                params: vec![],
                label: Some("receipt-unreadable-note-control".into()),
            })
            .await
            .unwrap();
        let error = classified_visibility_receipt(&runtime, &token, holder.id, true)
            .await
            .unwrap_err();
        let value = crate::error_projection::runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(value["details"]["reason"], "receipt_store_unavailable");
        assert_eq!(value["details"]["memory_id"], holder.id.to_string());
        assert_eq!(value["domain_disposition"], "unknown");
        assert_eq!(value["retryable"], true);
        for private in [KEY, CONTENT, "private_unreadable_notes"] {
            assert!(!value.to_string().contains(private));
        }
    }
}
