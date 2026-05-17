//! Parameter types for the `list` verb (ADR-023).

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Input for `list` — list records with optional filtering.
///
/// kind="entity": returns entities, optionally filtered by entity_kind.
/// kind="edge": returns edges, optionally filtered by source/target/relations/weight.
/// kind="note": returns notes, optionally filtered by note_kind.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    /// Discriminant. One of: entity | edge | note
    #[schemars(description = "entity | edge | note")]
    pub kind: String,

    /// Namespace (omit for server default).
    pub namespace: Option<String>,

    /// Maximum results to return. Defaults: entity=50 (max 500), edge=100 (max 1000), note=20 (max 200).
    pub limit: Option<u32>,

    // ---- entity-specific filter ----
    /// Filter entities by entity_kind: concept | document | dataset | project | person | org
    #[schemars(description = "concept | document | dataset | project | person | org")]
    pub entity_kind: Option<String>,

    // ---- edge-specific filters ----
    /// Only return edges whose source is this UUID.
    pub source_id: Option<String>,

    /// Only return edges whose target is this UUID.
    pub target_id: Option<String>,

    /// Filter to these relations only. Omit or empty = any relation.
    /// Valid: contains | part_of | instance_of | extends | variant_of | introduced_by |
    ///        supersedes | depends_on | enables | implements | competes_with | composed_with | annotates
    pub relations: Option<Vec<String>>,

    /// Minimum edge weight (inclusive).
    pub min_weight: Option<f64>,

    /// Maximum edge weight (inclusive).
    pub max_weight: Option<f64>,

    // ---- note-specific filter ----
    /// Filter notes by note_kind: observation | insight | question | decision | reference
    #[schemars(description = "observation | insight | question | decision | reference")]
    pub note_kind: Option<String>,
}
