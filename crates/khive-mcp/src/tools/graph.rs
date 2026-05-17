//! MCP tool parameter types for graph (edge) operations.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `link` — create a directed edge between two entities.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LinkParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the source entity.
    pub source_id: String,

    /// UUID of the target entity.
    pub target_id: String,

    /// Edge relation. Must be one of the 13 canonical relations (ADR-002):
    ///
    /// Structure:    contains | part_of | instance_of
    /// Derivation:   extends | variant_of | introduced_by | supersedes
    /// Dependency:   depends_on | enables
    /// Implementation: implements
    /// Lateral:      competes_with | composed_with
    /// Annotation:   annotates
    #[schemars(
        description = "One of: contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | depends_on | enables | implements | competes_with | composed_with | annotates"
    )]
    pub relation: String,

    /// Edge weight between 0.0 and 1.0.
    /// 1.0 = definitional, 0.7-0.9 = strong, 0.4-0.6 = plausible, <0.4 = speculative.
    /// Defaults to 1.0.
    pub weight: Option<f64>,
}

/// Input for `neighbors` — get immediate neighbors of a node.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NeighborsParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the node to query.
    pub node_id: String,

    /// Traversal direction: out | in | both (default: out).
    #[schemars(description = "out | in | both  (default: out)")]
    pub direction: Option<String>,

    /// Maximum neighbors to return (default: no limit).
    pub limit: Option<u32>,

    /// Restrict to these edge relations only (e.g. ["annotates"]). Omit for all relations.
    pub relations: Option<Vec<String>>,
}

/// Input for `traverse` — multi-hop graph traversal.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TraverseParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Root node UUIDs to start traversal from.
    pub roots: Vec<String>,

    /// Maximum hop depth (default: 3).
    pub max_depth: Option<usize>,

    /// Traversal direction: out | in | both (default: out).
    #[schemars(description = "out | in | both  (default: out)")]
    pub direction: Option<String>,

    /// Restrict traversal to these relations only. Omit for all relations.
    pub relations: Option<Vec<String>>,

    /// Whether to include root nodes in results (default: true).
    pub include_roots: Option<bool>,
}
