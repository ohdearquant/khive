//! `comm.withheld_body` -- read the original bytes of a delivered withheld message.
//!
//! A screening rule can withhold an external message's body and deliver only a
//! placeholder. The original RFC 5322 bytes stay in content-addressed storage,
//! owned by the delivered message's `quarantine-original` attachment. This verb
//! is the one read for those bytes: it is open to the message's recipient actor
//! and to the maintainer (the runtime's configured actor), it refuses every
//! other caller before the original is read, and it never returns a content
//! reference. Every call, allowed or refused, appends exactly one audit event
//! naming the caller, the message id when one resolved, and the outcome.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{AttachmentSubstrate, Event, StorageError};
use khive_types::{EventKind, KhiveError, SubstrateKind};

pub(crate) const WITHHELD_BODY_VERB: &str = "comm.withheld_body";

const OUTCOME_ALLOWED: &str = "allowed";
const OUTCOME_NOT_FOUND: &str = "not_found";
const OUTCOME_REFUSED_CALLER: &str = "refused_caller";
const OUTCOME_REFUSED_NOT_WITHHELD: &str = "refused_not_withheld";
const OUTCOME_ORIGINAL_UNAVAILABLE: &str = "original_unavailable";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WithheldBodyParams {
    id: String,
}

/// Append the one audit event an outcome owes. The caller is the event actor,
/// the message id (when one resolved) is the event target, and `outcome` is
/// the payload token. The payload never carries the sender, the subject, or any
/// part of the original.
async fn audit_outcome(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    target: Option<Uuid>,
    outcome: &str,
) -> Result<Uuid, RuntimeError> {
    let mut event = Event::new(
        token.namespace().as_str(),
        WITHHELD_BODY_VERB,
        EventKind::Audit,
        SubstrateKind::Event,
        token.actor().id.clone(),
    )
    .with_payload(json!({
        "action": "withheld_body_read",
        "outcome": outcome,
    }));
    if let Some(id) = target {
        event = event.with_target(id);
    }
    let audit_event_id = event.id;
    runtime
        .events(token)?
        .append_event(event)
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(audit_event_id)
}

/// A delivered withheld message: a message note whose body screening replaced
/// with a rule-ID placeholder, delivered rather than quarantined, from an
/// external sender we do not trust. A quarantine record carries `quarantined`
/// and is refused here.
fn is_delivered_withheld_message(note: &khive_storage::Note) -> bool {
    if note.kind != "message" {
        return false;
    }
    let Some(props) = note.properties.as_ref() else {
        return false;
    };
    props.get("screening_body_withheld") == Some(&Value::Bool(true))
        && !(props.get("quarantined") == Some(&Value::Bool(true))
            || props.get("quarantined").and_then(Value::as_str) == Some("true"))
        && props.get("origin").and_then(Value::as_str) == Some("external")
        && props.get("trust").and_then(Value::as_str) == Some("untrusted")
}

/// Replace a blob read failure with an error that names the message id only.
///
/// Storage errors name the object's content reference, and this verb never
/// returns one, so none of them is passed through. An original larger than the
/// whole-object read limit is refused by that limit, never returned truncated.
fn original_read_error(id: Uuid, error: StorageError) -> RuntimeError {
    match error {
        StorageError::BlobTooLarge { max_bytes, .. } => RuntimeError::InvalidInput(format!(
            "withheld_body: the original of message {id} exceeds the {max_bytes}-byte read limit"
        )),
        StorageError::NotFound { .. } => RuntimeError::NotFound(format!(
            "withheld_body: the original of message {id} is not in the blob store"
        )),
        other => {
            tracing::warn!(message_id = %id, error = %other, "withheld_body: original read failed");
            RuntimeError::Internal(format!(
                "withheld_body: the original of message {id} could not be read"
            ))
        }
    }
}

/// Read the original the message's `quarantine-original` attachment owns.
///
/// The read is verified and bounded: a body larger than the store's whole-object
/// ceiling is refused by name, never returned truncated.
async fn read_original(runtime: &KhiveRuntime, id: Uuid) -> Result<Vec<u8>, RuntimeError> {
    let store = runtime.require_blob_store()?;
    let owner = runtime
        .core()
        .attachments()?
        .get_attachment(id, "quarantine-original")
        .await?
        .ok_or_else(|| {
            RuntimeError::NotFound(format!(
                "withheld_body: message {id} retains no original body"
            ))
        })?;
    if owner.substrate != AttachmentSubstrate::Note {
        return Err(RuntimeError::Internal(format!(
            "withheld_body: message {id} has a malformed original owner"
        )));
    }
    store
        .get_bounded_verified(&owner.content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
        .await
        .map_err(|error| original_read_error(id, error))
}

/// `comm.withheld_body` -- read one delivered withheld message's original bytes,
/// base64-encoded with their size.
///
/// Check order matters: the caller check runs before the withheld check, so a
/// third actor cannot learn from the error whether a message was withheld.
pub(crate) async fn handle_withheld_body(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: WithheldBodyParams = khive_runtime::deser_params(params)?;

    // 1. Resolve the id in the caller's namespace. An id that is malformed or
    //    matches no record, or resolves to a note this namespace does not hold,
    //    is NotFound. An ambiguous prefix and a storage failure propagate as
    //    they are.
    let id = match runtime
        .resolve_uuid_or_prefix_for_verb(token, params.id.trim(), WITHHELD_BODY_VERB)
        .await
    {
        Ok(id) => id,
        Err(RuntimeError::InvalidInput(_)) => {
            audit_outcome(runtime, token, None, OUTCOME_NOT_FOUND).await?;
            return Err(RuntimeError::NotFound(
                "withheld_body: message not found".into(),
            ));
        }
        Err(error) => return Err(error),
    };
    let note = match runtime.notes(token)?.get_note(id).await? {
        Some(note) if note.namespace == token.namespace().as_str() => note,
        _ => {
            audit_outcome(runtime, token, Some(id), OUTCOME_NOT_FOUND).await?;
            return Err(RuntimeError::NotFound(format!(
                "withheld_body: message {id} not found"
            )));
        }
    };

    // 2. Only the recipient actor and the maintainer may read a withheld
    //    original. This check precedes the withheld check on purpose: it must
    //    not disclose the message's disposition to anyone else.
    let caller = token.actor().id.as_str();
    let recipient = note
        .properties
        .as_ref()
        .and_then(|props| props.get("to_actor"))
        .and_then(Value::as_str);
    let maintainer = runtime.config().actor_id.as_deref();
    if recipient != Some(caller) && maintainer != Some(caller) {
        audit_outcome(runtime, token, Some(id), OUTCOME_REFUSED_CALLER).await?;
        return Err(RuntimeError::Khive(KhiveError::unauthorized(
            "withheld_body: caller is neither the recipient nor the maintainer of this message",
        )));
    }

    // 3. Only a delivered withheld external message has an original to read; a
    //    quarantine record's own recovery path keeps it.
    if !is_delivered_withheld_message(&note) {
        audit_outcome(runtime, token, Some(id), OUTCOME_REFUSED_NOT_WITHHELD).await?;
        return Err(RuntimeError::InvalidInput(
            "withheld_body: message is not a delivered withheld external message".into(),
        ));
    }

    // 4. Read the original through the message's owner attachment.
    let bytes = match read_original(runtime, id).await {
        Ok(bytes) => bytes,
        Err(error) => {
            audit_outcome(runtime, token, Some(id), OUTCOME_ORIGINAL_UNAVAILABLE).await?;
            return Err(error);
        }
    };

    // 5. Success: the bytes and their size, and no content reference anywhere.
    let audit_event_id = audit_outcome(runtime, token, Some(id), OUTCOME_ALLOWED).await?;
    Ok(json!({
        "id": id.as_hyphenated().to_string(),
        "size_bytes": bytes.len(),
        "original_base64": BASE64.encode(&bytes),
        "audit_event_id": audit_event_id,
    }))
}

#[cfg(test)]
#[path = "withheld_body_tests.rs"]
mod tests;
