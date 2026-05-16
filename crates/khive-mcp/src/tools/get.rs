//! Parameter types for the `get` verb (ADR-023).

use rmcp::schemars;
use serde::Deserialize;

/// Input for `get` — fetch any single record by UUID.
///
/// Automatically determines whether the UUID refers to an entity, note, or edge.
/// Returns `{"kind": "entity"|"note"|"edge", "data": {...}}` if found, or an error if not found.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the record to fetch (full UUID or 8-char short form).
    pub id: String,
}
