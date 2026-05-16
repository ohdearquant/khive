// Licensed under the Apache License, Version 2.0.

//! MCP tool parameter types for edge CRUD operations (ADR-014).

use rmcp::schemars;
use serde::Deserialize;

/// Input for `edge_get` — fetch a single edge by UUID.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeGetParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Edge UUID.
    pub id: String,
}

/// Input for `edge_list` — list edges with optional filtering.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeListParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Only return edges whose source is this entity UUID.
    pub source_id: Option<String>,

    /// Only return edges whose target is this entity UUID.
    pub target_id: Option<String>,

    /// Filter to these relations only. Omit or empty = any relation.
    pub relations: Option<Vec<String>>,

    /// Minimum edge weight (inclusive).
    pub min_weight: Option<f64>,

    /// Maximum edge weight (inclusive).
    pub max_weight: Option<f64>,

    /// Maximum edges to return. Default 100, max 1000.
    pub limit: Option<u32>,
}

/// Input for `edge_update` — patch-style edge modification.
///
/// Only the fields you provide are changed.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeUpdateParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Edge UUID to update.
    pub id: String,

    /// New relation. Must be one of the 13 canonical ADR-002 relations. Omit to leave unchanged.
    #[schemars(
        description = "One of: contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | depends_on | enables | implements | competes_with | composed_with | annotates"
    )]
    pub relation: Option<String>,

    /// New weight in [0.0, 1.0]. Omit to leave unchanged.
    pub weight: Option<f64>,
}

/// Input for `edge_delete` — hard-delete an edge by UUID.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EdgeDeleteParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Edge UUID to delete.
    pub id: String,
}
