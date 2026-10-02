//! Ordered, local-message attachment metadata over the existing substrate.

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{Attachment, AttachmentSubstrate, ContentRef, NewAttachment};
use serde_json::{json, Value};
use std::collections::HashSet;
use uuid::Uuid;

const ROLE_PREFIX: &str = "message-attachment:";
const MAX_ATTACHMENTS: usize = 8;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

fn role_index(role: &str) -> Option<u8> {
    match role.strip_prefix(ROLE_PREFIX)?.as_bytes() {
        [index @ b'0'..=b'7'] => Some(index - b'0'),
        _ => None,
    }
}

pub(crate) async fn prepare(
    runtime: &KhiveRuntime,
    verb: &str,
    recipient: &str,
    references: &[String],
) -> Result<Vec<NewAttachment>, RuntimeError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    if references.len() > MAX_ATTACHMENTS {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: at most 8 attachments are permitted"
        )));
    }
    if ["email:", "telegram:", "khive1:"]
        .iter()
        .any(|prefix| recipient.starts_with(prefix))
    {
        return Err(RuntimeError::InvalidInput(format!("{verb}: attachments require a local recipient; {recipient:?} is an outbound channel address")));
    }
    // Pins note and attachment SQL to main before preparing any write.
    runtime.attachments().map_err(|error| {
        RuntimeError::InvalidInput(format!(
            "{verb}: attachments require the canonical main comm backend: {error}"
        ))
    })?;
    let store = runtime.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured("comm attachments require an installed BlobStore".into())
    })?;
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    let mut attachments = Vec::with_capacity(references.len());
    for (index, raw) in references.iter().enumerate() {
        let content_ref = ContentRef::from_hex(raw).map_err(|error| {
            RuntimeError::InvalidInput(format!("{verb}: invalid attachment {raw:?}: {error}"))
        })?;
        if !seen.insert(content_ref.clone()) {
            return Err(RuntimeError::InvalidInput(format!(
                "{verb}: duplicate attachment {content_ref}"
            )));
        }
        // A metadata pre-check does not reserve an independently routed object.
        let size = store.size(&content_ref).await?.ok_or_else(|| {
            RuntimeError::InvalidInput(format!(
                "{verb}: no object exists for attachment {content_ref}"
            ))
        })?;
        total = total
            .checked_add(size)
            .filter(|total| *total <= MAX_TOTAL_BYTES)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "{verb}: attachment total exceeds the {MAX_TOTAL_BYTES}-byte maximum"
                ))
            })?;
        attachments.push(NewAttachment {
            role: format!("{ROLE_PREFIX}{index}"),
            content_ref,
            media_type: None,
            size_bytes: Some(size),
        });
    }
    Ok(attachments)
}

pub(crate) async fn rows(
    runtime: &KhiveRuntime,
    id: Uuid,
) -> Result<Vec<Attachment>, RuntimeError> {
    // Ordinary split-backend messages have no file attachments; never cross databases.
    if runtime.backend_id().as_str() != khive_runtime::BackendId::MAIN {
        return Ok(Vec::new());
    }
    let mut rows = runtime.attachments()?.list_attachments(id).await?;
    rows.retain(|row| {
        row.substrate == AttachmentSubstrate::Note && role_index(&row.role).is_some()
    });
    rows.sort_by_key(|row| role_index(&row.role));
    Ok(rows)
}

pub(crate) async fn metadata(runtime: &KhiveRuntime, id: Uuid) -> Result<Value, RuntimeError> {
    Ok(json!(rows(runtime, id)
        .await?
        .into_iter()
        .map(|row| json!({
            "content_ref": row.content_ref, "size": row.size_bytes, "media_type": row.media_type,
        }))
        .collect::<Vec<_>>()))
}

pub(crate) async fn enrich(
    runtime: &KhiveRuntime,
    message: &mut Value,
) -> Result<(), RuntimeError> {
    let id = message["full_id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| {
            RuntimeError::Internal("message attachment view has no canonical owner UUID".into())
        })?;
    message["attachments"] = metadata(runtime, id).await?;
    Ok(())
}

pub(crate) fn identify_request(mut request: Value, references: &[String]) -> Value {
    // Preserve identity JSON for existing attachment-free keys.
    if !references.is_empty() {
        request["attachments"] = json!(references);
    }
    request
}
