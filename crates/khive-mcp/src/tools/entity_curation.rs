// Licensed under the Apache License, Version 2.0.

//! MCP tool parameter types for entity curation operations (ADR-014).

use rmcp::schemars;
use serde::Deserialize;

/// Input for `entity_update` — patch-style entity modification.
///
/// Only the fields you provide are changed. Omitted fields leave the entity unchanged.
/// For `description`: omit the key = leave unchanged, `null` = clear, string = set.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityUpdateParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Entity UUID to update.
    pub id: String,

    /// New name for the entity. Omit to leave unchanged.
    pub name: Option<String>,

    /// New description. Omit = unchanged. Set to `null` to clear. Set to a string to replace.
    #[schemars(description = "Omit=unchanged, null=clear, string=set")]
    pub description: Option<serde_json::Value>,

    /// Wholesale replace the properties object. Omit to leave unchanged.
    pub properties: Option<serde_json::Value>,

    /// Wholesale replace the tags list. Omit to leave unchanged.
    pub tags: Option<Vec<String>>,
}

/// Input for `entity_merge` — merge two entities, rewiring all edges.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityMergeParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the entity to keep. All edges are rewired to this entity.
    pub into_id: String,

    /// UUID of the entity to absorb and delete.
    pub from_id: String,

    /// Conflict resolution strategy for properties.
    ///
    /// - `prefer_into` (default): `into` values win; `from` fills in missing keys.
    /// - `prefer_from`: `from` values win on conflict.
    /// - `union`: deep object merge; scalar conflicts go to `into`.
    #[schemars(description = "prefer_into (default) | prefer_from | union")]
    pub strategy: Option<String>,
}
