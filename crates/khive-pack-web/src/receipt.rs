//! Receipt writing for network actions and capture-bound extraction.
//!
//! A receipt is a `kind = "observation"` note carrying a structured
//! `properties["request"]` block — a note, not a new kind, matching the base
//! contract's `note annotates *` row (ADR-002:503, note→any). D4: "every
//! network action writes one observation note annotating the entity it
//! touched (or standing alone for a search)" — `annotates` is therefore
//! empty for a transient `web.fetch` or a `web.search` without persisted
//! hits. Chained by `supersedes` (D2's receipt-chain row) is the
//! caller's job: `write_receipt` returns the new note's id so the caller can
//! link it to the previous receipt for the same resource.

pub const RECEIPT_TAG: &str = "web.receipt";
pub const EXTRACTION_RECEIPT_TAG: &str = "web.extraction";
pub const RECEIPT_PROVENANCE_KEY: &str = khive_runtime::secret_gate::RESERVED_WEB_RECEIPT_KEY;
pub const RECEIPT_PROVENANCE_VALUE: &str = khive_runtime::secret_gate::WEB_RECEIPT_PROVENANCE_VALUE;

use khive_runtime::{EntityPatch, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{
    Attachment, AttachmentSubstrate, ContentRef, Direction, EdgeRelation, NewAttachment,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use uuid::Uuid;

pub(crate) fn has_receipt_provenance(properties: Option<&Value>) -> bool {
    properties
        .and_then(|properties| properties.get(RECEIPT_PROVENANCE_KEY))
        .and_then(Value::as_str)
        == Some(RECEIPT_PROVENANCE_VALUE)
}

/// Find the receipt that stored the exact body selected by `web.extract`.
/// A newer HEAD or 304 may annotate the same document without carrying that
/// body; `capture_receipt_id` on the document is only a hint. A receipt that
/// annotates several redirect participants must prove which row owns its body.
pub(crate) async fn capture_for_body(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    entity: &khive_storage::Entity,
    content_ref: &str,
) -> Result<Option<(Uuid, Value)>, RuntimeError> {
    let latest = runtime
        .latest_annotating_note_with_property(
            token,
            entity.id,
            "observation",
            RECEIPT_TAG,
            RECEIPT_PROVENANCE_KEY,
            RECEIPT_PROVENANCE_VALUE,
        )
        .await?;
    let stored = entity
        .properties
        .as_ref()
        .and_then(|properties| properties.get("capture_receipt_id"))
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let mut cursor = latest;
    let mut visited = HashSet::new();
    while let Some(id) = cursor {
        if !visited.insert(id) || visited.len() > 1_000 {
            break;
        }
        let Some(note) = runtime.notes(token)?.get_note(id).await? else {
            break;
        };
        if note.namespace == token.namespace().as_str()
            && note.kind == "observation"
            && has_receipt_provenance(note.properties.as_ref())
            && note.properties.as_ref().is_some_and(|properties| {
                properties["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(RECEIPT_TAG)))
            })
        {
            let request = &note.properties.as_ref().expect("checked above")["request"];
            if receipt_owns_body(request, entity.id, content_ref)
                && receipt_annotates_document(runtime, token, id, entity.id).await?
            {
                return Ok(Some((id, request.clone())));
            }
        }
        cursor = runtime
            .neighbors(
                token,
                id,
                Direction::Out,
                Some(1),
                Some(vec![EdgeRelation::Supersedes]),
            )
            .await?
            .first()
            .map(|neighbor| neighbor.node_id);
    }
    if let Some(id) = stored {
        let Some(note) = runtime.notes(token)?.get_note(id).await? else {
            return Ok(None);
        };
        if note.namespace == token.namespace().as_str()
            && note.kind == "observation"
            && has_receipt_provenance(note.properties.as_ref())
            && note.properties.as_ref().is_some_and(|properties| {
                properties["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some(RECEIPT_TAG)))
            })
        {
            let request = &note.properties.as_ref().expect("checked above")["request"];
            if receipt_owns_body(request, entity.id, content_ref)
                && receipt_annotates_document(runtime, token, id, entity.id).await?
            {
                return Ok(Some((id, request.clone())));
            }
        }
    }
    Ok(None)
}

fn receipt_owns_body(request: &Value, entity_id: Uuid, content_ref: &str) -> bool {
    request["content_ref"].as_str() == Some(content_ref)
        && request["body_entity_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            == Some(entity_id)
}

async fn receipt_annotates_document(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    receipt_id: Uuid,
    document_id: Uuid,
) -> Result<bool, RuntimeError> {
    Ok(runtime
        .neighbors(
            token,
            receipt_id,
            Direction::Out,
            None,
            Some(vec![EdgeRelation::Annotates]),
        )
        .await?
        .iter()
        .any(|neighbor| neighbor.node_id == document_id))
}

/// Keep the convenience pointer only if the document holds this exact body
/// in both its representation properties and canonical content attachment.
/// The graph-row CAS catches a concurrent representation replacement. On a
/// split backend, the main attachment read and routed CAS are separate, so
/// an attachment-only change between them remains a residual race.
pub(crate) async fn bind_capture_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    entity_id: Uuid,
    content_ref: &str,
    receipt_id: Uuid,
) -> Result<bool, RuntimeError> {
    let Some(entity) = runtime.entities(token)?.get_entity(entity_id).await? else {
        return Ok(false);
    };
    crate::entities::require_entity_namespace(token, &entity)?;
    // The routed web entity has no local projection of the canonical main
    // attachment. Check the authoritative root, then CAS the graph row below.
    let attachment = runtime
        .core()
        .attachments()?
        .get_attachment(entity_id, "content")
        .await?;
    if attachment
        .as_ref()
        .filter(|attachment| attachment.substrate == AttachmentSubstrate::Entity)
        .map(|attachment| attachment.content_ref.as_str())
        != Some(content_ref)
        || entity
            .properties
            .as_ref()
            .and_then(|properties| properties.get("blob_ref"))
            .and_then(Value::as_str)
            != Some(content_ref)
    {
        return Ok(false);
    }
    match runtime
        .update_entity_if_unchanged(
            token,
            &entity,
            EntityPatch {
                properties: Some(json!({ "capture_receipt_id": receipt_id.to_string() })),
                ..Default::default()
            },
            &[],
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(RuntimeError::Khive(error)) if error.kind() == khive_types::ErrorKind::Conflict => {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Write a receipt note and return its id.
///
/// `summary` becomes the note's searchable `content`; `request` is the
/// structured block callers reconstruct the decision from later; `annotates`
/// names the entity (or entities) this network action touched.
pub async fn write_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    summary: &str,
    request: Value,
    annotates: Vec<Uuid>,
) -> Result<Uuid, RuntimeError> {
    let note = runtime
        .create_web_receipt_note(token, summary, request, annotates)
        .await?;
    Ok(note.id)
}

/// Record one extraction independently of the network receipt chain.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_extraction_receipt(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    document_id: Uuid,
    request: Value,
    mut annotates: Vec<Uuid>,
    source_content_ref: &ContentRef,
    source_size: u64,
    source_media_type: Option<&str>,
) -> Result<Uuid, RuntimeError> {
    annotates.insert(0, document_id);
    let properties = json!({
        "tags": [EXTRACTION_RECEIPT_TAG],
        "request": request,
    });
    let note = runtime
        .create_note(
            token,
            "observation",
            None,
            "web.extract",
            None,
            Some(properties),
            annotates,
        )
        .await?;
    let attachment = Attachment::from_new(
        note.id,
        AttachmentSubstrate::Note,
        NewAttachment {
            role: "source".to_string(),
            content_ref: source_content_ref.clone(),
            media_type: source_media_type.map(str::to_owned),
            size_bytes: Some(source_size),
        },
        chrono::Utc::now().timestamp_micros(),
    );
    attachment.validate().map_err(|error| {
        RuntimeError::Internal(format!("extraction source attachment invalid: {error}"))
    })?;
    runtime
        .core()
        .attachments()?
        .upsert_attachment(attachment)
        .await
        .map_err(|error| {
            RuntimeError::Internal(format!(
                "extraction source attachment write failed: {error}"
            ))
        })?;
    Ok(note.id)
}
