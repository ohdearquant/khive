//! Parameter types for the `delete` verb (ADR-023).

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `delete` — remove a record by UUID.
///
/// The record kind (entity, edge, or note) is determined automatically from the UUID.
/// Entity and note: soft-delete by default; set hard=true for permanent removal.
/// Edge: always hard-deleted (edges have no soft-delete state).
///
/// Returns {"deleted": true, "id": "<uuid>"} on success.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the record to delete.
    pub id: String,

    /// If true, permanently remove the record. Default false (soft-delete).
    /// Ignored for edges — edges are always hard-deleted.
    pub hard: Option<bool>,
}
