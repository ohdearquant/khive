//! Parameter types for the `search` verb (ADR-023, ADR-024).

use rmcp::schemars;
use serde::Deserialize;

/// Input for `search` — semantic/hybrid search across entities or notes.
///
/// kind="entity": hybrid search (FTS5 + optional vector) against the entity store.
/// kind="note": hybrid search with salience weighting (ADR-024 pipeline).
///
/// Both pipelines run FTS5 text search + optional vector similarity (if an embedding
/// model is configured), fused via Reciprocal Rank Fusion (k=60).
/// Note search additionally applies salience weighting: score *= (0.5 + 0.5 * salience).
///
/// Soft-deleted and superseded notes are excluded from results.
///
/// Examples:
///   Find entities: {"kind":"entity","query":"FlashAttention memory efficient attention"}
///   Find notes:    {"kind":"note","query":"LoRA fine-tuning parameter efficiency","limit":5}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Discriminant. One of: entity | note
    #[schemars(description = "entity | note")]
    pub kind: String,

    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Search query string (natural language or keyword).
    pub query: String,

    /// Maximum results to return. Default 10, max 100.
    pub limit: Option<u32>,
}
