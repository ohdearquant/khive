//! Additive, caller-attributed traces for refused atom batches. Atom rows are never written here.

use super::schema::{Atom, AtomWrite};
use super::util::{atom_from_row, has_domain_mirror_tag, validate_atom_content};
use khive_runtime::{
    secret_gate, KhiveRuntime, NamespaceToken, RefusalEventRecording, RefusalRecordingErrorClass,
    RuntimeError,
};
use khive_storage::{Event, SqlStatement, SqlValue};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};

pub(super) struct SubmissionRefusal {
    pub source: RuntimeError,
    pub field: &'static str,
}

pub(super) fn validate_submission(
    atom: &AtomWrite,
    index: usize,
    preserve_content_whitespace: bool,
) -> Result<(), RuntimeError> {
    validate_submission_located(atom, index, preserve_content_whitespace)
        .map_err(|failure| failure.source)
}

pub(super) fn validate_submission_located(
    atom_in: &AtomWrite,
    index: usize,
    preserve_content_whitespace: bool,
) -> Result<(), SubmissionRefusal> {
    fn at(result: Result<(), RuntimeError>, field: &'static str) -> Result<(), SubmissionRefusal> {
        result.map_err(|source| SubmissionRefusal { source, field })
    }
    let record = format!("atoms[{index}]");
    let atom_in = match atom_in {
        AtomWrite::Upsert(atom_in) => atom_in,
        AtomWrite::PropertiesOnly(atom_in) => {
            at(
                secret_gate::check_json_at(&atom_in.properties, &record, "properties"),
                "properties",
            )?;
            return at(
                secret_gate::reject_reserved_secret_gate_property(Some(&atom_in.properties)),
                "properties",
            );
        }
    };
    let slug = atom_in.slug.trim();
    if slug.is_empty() {
        return Err(SubmissionRefusal {
            source: RuntimeError::InvalidInput("atom slug must not be empty".into()),
            field: "slug",
        });
    }
    let raw_content = atom_in.content.as_deref().unwrap_or("");
    let content = if preserve_content_whitespace {
        raw_content
    } else {
        raw_content.trim()
    };
    at(validate_atom_content(content), "content")?;
    at(secret_gate::check_at(slug, &record, "slug"), "slug")?;
    at(
        secret_gate::check_at(&atom_in.name, &record, "name"),
        "name",
    )?;
    at(
        secret_gate::check_at(content, &record, "content"),
        "content",
    )?;
    if let Some(tags) = &atom_in.tags {
        at(secret_gate::check_tags_at(tags, &record, "tags"), "tags")?;
    }
    if let Some(properties) = &atom_in.properties {
        at(
            secret_gate::check_json_at(properties, &record, "properties"),
            "properties",
        )?;
    }
    at(
        secret_gate::reject_reserved_secret_gate_property(atom_in.properties.as_ref()),
        "properties",
    )?;
    if let Some(Some(uri)) = &atom_in.source_uri {
        at(
            secret_gate::check_at(uri, &record, "source_uri"),
            "source_uri",
        )?;
    }
    if let Some(Some(source_type)) = &atom_in.source_type {
        at(
            secret_gate::check_at(source_type, &record, "source_type"),
            "source_type",
        )?;
    }
    Ok(())
}

/// Return the first original refusal, augmented only by confirmed or failed trace attempts.
/// Import's pre-existing refusal policy does not append these public-upsert events.
pub(super) async fn refuse_batch(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    atoms: &[Option<AtomWrite>],
    mut failures: Vec<Option<RuntimeError>>,
    is_import: bool,
) -> RuntimeError {
    let first_index = failures
        .iter()
        .position(Option::is_some)
        .expect("refuse_batch requires an original refusal");
    if is_import {
        return failures[first_index].take().expect("first refusal");
    }
    // Read every target before opening an event writer. These lookups do not create,
    // revive, deprecate or otherwise mutate the serving records.
    let sql = runtime.sql();
    let mut reader = match sql.reader().await {
        Ok(reader) => reader,
        Err(_) => {
            tracing::warn!(
                operation = "knowledge.upsert_atoms",
                error_class = "refusal_target_lookup_failed",
                "refused atom batch: target lookup unavailable"
            );
            return failures[first_index].take().expect("first refusal");
        }
    };
    let mut targets = Vec::with_capacity(atoms.len());
    for atom in atoms {
        let Some(atom) = atom else {
            targets.push(None);
            continue;
        };
        let (predicate, params) = match atom {
            AtomWrite::Upsert(input) => (
                "a.namespace = ?1 AND a.slug = ?2",
                vec![
                    SqlValue::Text(token.namespace().as_str().to_owned()),
                    SqlValue::Text(input.slug.trim().to_owned()),
                ],
            ),
            AtomWrite::PropertiesOnly(input) => {
                ("a.id = ?1", vec![SqlValue::Text(input.id.to_string())])
            }
        };
        let row = reader.query_row(SqlStatement {
            sql: format!("SELECT a.* FROM knowledge_atoms a WHERE {predicate} AND a.deleted_at IS NULL AND NOT EXISTS (SELECT 1 FROM knowledge_domains d WHERE d.id = a.id) LIMIT 1"),
            params, label: Some("knowledge.upsert_atoms.refusal_target".into()),
        }).await;
        let target = match row {
            Ok(Some(row)) => atom_from_row(&row).filter(|old| !has_domain_mirror_tag(&old.tags)),
            Ok(None) => None,
            Err(_) => {
                tracing::warn!(
                    operation = "knowledge.upsert_atoms",
                    error_class = "refusal_target_lookup_failed",
                    "refused atom batch: target lookup unavailable"
                );
                None
            }
        };
        targets.push(target);
    }
    drop(reader);

    let mut recordings = Vec::new();
    for (index, (input, target)) in atoms.iter().zip(targets).enumerate() {
        let (Some(input), Some(old)) = (input, target) else {
            continue;
        };
        let digest = secret_gate::masked_submitted_atom_digest_v1(&candidate(input, &old));
        let mut payload = json!({
            "subject_kind": "knowledge_atom", "item_index": index,
            "digest_input": "masked_submitted_atom_v1", "rejected_digest": digest,
        });
        let outcome = if let Some(error) = &failures[index] {
            match error {
                RuntimeError::SecretDetected(found) => {
                    payload["reason"] = json!("secret_detected");
                    payload["detector"] = json!(found.detector);
                    payload["trigger"] = json!(found.trigger);
                    payload["location"] = json!(found.location);
                    EventOutcome::Denied
                }
                _ => {
                    payload["reason"] = json!("validation_refused");
                    EventOutcome::Error
                }
            }
        } else {
            payload["reason"] = json!("batch_refused");
            payload["first_refusing_item_index"] = json!(first_index);
            EventOutcome::Error
        };
        let event = Event::new(
            token.namespace().as_str(),
            "knowledge.upsert_atoms",
            EventKind::Refusal,
            SubstrateKind::Event,
            format!("{}:{}", token.actor().kind, token.actor().id),
        )
        .with_target(old.id)
        .with_outcome(outcome)
        .with_payload(payload);
        let event_id = event.id;
        match runtime.events(token) {
            Ok(store) => match store.append_event(event).await {
                Ok(()) => recordings.push(RefusalEventRecording::Recorded {
                    item_index: index,
                    subject: old.id,
                    event_id,
                }),
                Err(_) => record_failure(
                    &mut recordings,
                    index,
                    old.id,
                    RefusalRecordingErrorClass::EventAppendFailed,
                ),
            },
            Err(_) => record_failure(
                &mut recordings,
                index,
                old.id,
                RefusalRecordingErrorClass::EventStoreUnavailable,
            ),
        }
    }
    failures[first_index]
        .take()
        .expect("first refusal")
        .with_refusal_events(recordings)
}

fn record_failure(
    recordings: &mut Vec<RefusalEventRecording>,
    item_index: usize,
    subject: uuid::Uuid,
    error_class: RefusalRecordingErrorClass,
) {
    // No input, digest, masked preview or raw storage error goes to the fallback log.
    tracing::warn!(operation = "knowledge.upsert_atoms", item_index, %subject,
        error_class = error_class.as_str(), "refused atom batch: refusal trace not confirmed");
    recordings.push(RefusalEventRecording::Failed {
        item_index,
        subject,
        error_class,
    });
}

/// Reconstruct submitted persisted fields without volatile timestamps or identity.
/// Omitted patch fields retain their stored value exactly as ordinary upsert does.
fn candidate(input: &AtomWrite, old: &Atom) -> Value {
    match input {
        AtomWrite::PropertiesOnly(input) => json!({
            "slug": old.slug, "name": old.name, "content": old.content,
            "tags": serde_json::from_str::<Value>(&old.tags).unwrap_or_else(|_| Value::String(old.tags.clone())), "properties": input.properties,
            "source_uri": old.source_uri, "source_type": old.source_type,
            "finalized": old.finalized,
        }),
        AtomWrite::Upsert(input) => {
            json!({
                "slug": input.slug.trim(), "name": input.name,
                "content": input.content.as_deref().unwrap_or("").trim(),
                "tags": input.tags.as_deref().unwrap_or(&[]),
                "properties": input.properties,
                "source_uri": patch_text(&input.source_uri, &old.source_uri),
                "source_type": patch_text(&input.source_type, &old.source_type),
                "finalized": input.finalized.map(|value| value.unwrap_or(false)).unwrap_or(old.finalized),
            })
        }
    }
}

fn patch_text(patch: &Option<Option<String>>, stored: &Option<String>) -> Option<String> {
    match patch {
        None => stored.clone(),
        Some(None) => None,
        Some(Some(value)) => {
            let value = value.trim();
            if value.is_empty() {
                stored.clone()
            } else {
                Some(value.to_owned())
            }
        }
    }
}

#[cfg(test)]
#[path = "refusal_tests.rs"]
mod tests;
