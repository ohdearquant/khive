//! MCP tool parameter types for entity operations.

use rmcp::schemars;
use serde::Deserialize;

/// Input for `entity_create`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityCreateParams {
    /// Namespace to create the entity in. Omit to use the server default.
    #[schemars(description = "Namespace (omit to use server default)")]
    pub namespace: Option<String>,

    /// Entity kind. One of: concept | document | dataset | project | person | org
    ///
    /// - concept: algorithms, techniques, models, research gaps, ideas
    /// - document: papers, preprints, reports, blog posts
    /// - dataset: benchmarks, corpora, evaluation sets
    /// - project: codebases, libraries, frameworks
    /// - person: researchers, engineers, authors
    /// - org: labs, companies, institutions
    #[schemars(description = "concept | document | dataset | project | person | org")]
    pub kind: String,

    /// Human-readable name for the entity (must be unique within namespace+kind).
    pub name: String,

    /// Optional description.
    pub description: Option<String>,

    /// Optional freeform JSON properties. Use for type, domain, authors, year, etc.
    pub properties: Option<serde_json::Value>,

    /// Optional tags for filtering.
    pub tags: Option<Vec<String>>,
}

/// Input for `entity_get`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityGetParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,
    /// Entity UUID (full or 8-char short form).
    pub id: String,
}

/// Input for `entity_list`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityListParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,
    /// Filter by kind. One of: concept | document | dataset | project | person | org
    pub kind: Option<String>,
    /// Maximum results to return (default 50, max 500).
    pub limit: Option<u32>,
}

/// Input for `entity_delete`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityDeleteParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,
    /// Entity UUID to delete.
    pub id: String,
    /// If true, permanently remove the record. If false (default), soft-delete.
    pub hard: Option<bool>,
}
