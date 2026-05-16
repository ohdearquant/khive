//! MCP tool parameter types for note operations.

use rmcp::schemars;
use serde::Deserialize;

/// Input for `note_create` — create a note in the knowledge graph.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoteCreateParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Note kind for grouping (e.g., "observation", "decision", "insight").
    /// Defaults to "observation".
    pub kind: Option<String>,

    /// The text content of the note.
    pub content: String,

    /// Salience score between 0.0 (forgettable) and 1.0 (critical).
    /// Defaults to 0.5.
    pub salience: Option<f64>,

    /// Free-form JSON properties to attach to the note.
    pub properties: Option<serde_json::Value>,

    /// UUIDs of entities or notes this note annotates (creates `annotates` edges).
    pub annotates: Option<Vec<String>>,
}

/// Input for `note_list` — list notes, optionally filtered by kind.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoteListParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Filter by kind. Omit to list all kinds.
    pub kind: Option<String>,

    /// Maximum notes to return (default: 20, max: 200).
    pub limit: Option<u32>,
}
