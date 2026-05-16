//! Parameter types for the `update` verb (ADR-023).

use rmcp::schemars;
use serde::Deserialize;

/// Input for `update` — patch-style modification of an entity or edge.
///
/// Only fields you provide are changed — omitted fields are left as-is (patch semantics per ADR-014).
/// The record kind (entity or edge) is determined automatically from the UUID.
///
/// entity fields:
///   - description: omit=unchanged, null=clear, string=set
///   - properties: wholesale replace if provided
///   - tags: wholesale replace if provided
///
/// edge fields:
///   - relation must be one of the 13 canonical ADR-002 relations
///   - weight: float in [0.0, 1.0]
///
/// Examples:
///   Rename entity:      {"id":"<uuid>","name":"NewName"}
///   Clear description:  {"id":"<uuid>","description":null}
///   Adjust edge weight: {"id":"<uuid>","weight":0.7}
///   Fix edge relation:  {"id":"<uuid>","relation":"extends"}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateParams {
    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// UUID of the record to update.
    pub id: String,

    // ---- entity patch fields ----
    /// New name for the entity. Omit to leave unchanged.
    pub name: Option<String>,

    /// New description. Omit=unchanged, null=clear, string=set.
    #[schemars(description = "Omit=unchanged, null=clear, string=set")]
    pub description: Option<serde_json::Value>,

    /// Wholesale replace the properties object. Omit to leave unchanged.
    pub properties: Option<serde_json::Value>,

    /// Wholesale replace the tags list. Omit to leave unchanged.
    pub tags: Option<Vec<String>>,

    // ---- edge patch fields ----
    /// New relation. Must be one of the 13 canonical ADR-002 relations. Omit to leave unchanged.
    /// Valid: contains | part_of | instance_of | extends | variant_of | introduced_by |
    ///        supersedes | depends_on | enables | implements | competes_with | composed_with | annotates
    #[schemars(
        description = "One of: contains | part_of | instance_of | extends | variant_of | introduced_by | supersedes | depends_on | enables | implements | competes_with | composed_with | annotates"
    )]
    pub relation: Option<String>,

    /// New weight in [0.0, 1.0]. Omit to leave unchanged.
    pub weight: Option<f64>,
}
