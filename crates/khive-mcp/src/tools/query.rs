//! MCP tool parameter types for graph query operations.

use rmcp::schemars;
use serde::Deserialize;

/// Input for `query` — run a GQL or SPARQL query against the knowledge graph.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// GQL or SPARQL query string.
    ///
    /// GQL example:
    ///   MATCH (a:concept)-[e:extends]->(b) RETURN a.name, b.name LIMIT 10
    ///
    /// SPARQL example:
    ///   SELECT ?a WHERE { ?a :kind "concept" . }
    pub query: String,
}
