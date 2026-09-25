//! Receipt writing for `web.fetch`/`web.search`/`web.refresh` (ADR-191 D4).
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

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde_json::{json, Value};
use uuid::Uuid;

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
    let properties = json!({
        "tags": [RECEIPT_TAG],
        "request": request,
    });
    let note = runtime
        .create_note(
            token,
            "observation",
            None,
            summary,
            None,
            Some(properties),
            annotates,
        )
        .await?;
    Ok(note.id)
}
