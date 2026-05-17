//! Parameter types for the `merge` verb (ADR-023).
//!
//! v0.1 scope: entity-only. Note merge is deferred.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `merge` — deduplicate two entity records into one (ADR-014).
///
/// **v0.1: entity-only.** Both IDs must refer to entities (note merge is deferred).
///
/// Rewires all edges from `from_id` to `into_id`, merges properties by strategy,
/// unions tags, then hard-deletes `from_id`.
///
/// Use when you discover two records describe the same thing (deduplication).
/// Compare with `supersede` which preserves the old record as history (deferred past v0.1).
///
/// strategy options:
///   prefer_into (default): into's values win on conflict; from fills in missing keys
///   prefer_from: from's values win on conflict
///   union: deep object merge; scalar conflicts go to into
///
/// Returns a summary: kept_id, removed_id, edges_rewired, properties_merged, tags_unioned.
///
/// Warning: not atomic in v0.1 — re-run with the same args to recover from mid-way failures.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MergeParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the entity to keep. All edges are rewired to this entity.
    pub into_id: String,

    /// UUID of the entity to absorb and delete.
    pub from_id: String,

    /// Conflict resolution strategy for properties.
    #[schemars(description = "prefer_into (default) | prefer_from | union")]
    pub strategy: Option<String>,
}
