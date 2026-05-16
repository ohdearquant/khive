//! Parameter types for the `create` verb (ADR-023).

use rmcp::schemars;
use serde::Deserialize;

/// Input for `create` — create an entity or note.
///
/// kind="entity": requires `name` and `entity_kind`.
/// kind="note": requires `content`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateParams {
    /// Discriminant. One of: entity | note
    #[schemars(description = "entity | note")]
    pub kind: String,

    /// Namespace (omit to use the server default).
    pub namespace: Option<String>,

    // ---- entity-only fields ----
    /// Entity name — human-readable, unique within namespace+entity_kind.
    /// Required when kind="entity".
    pub name: Option<String>,

    /// Entity kind. One of: concept | document | dataset | project | person | org
    ///
    /// - concept: algorithms, techniques, models, architectures, research gaps
    /// - document: papers, preprints, reports, blog posts
    /// - dataset: benchmarks, corpora, evaluation sets
    /// - project: codebases, libraries, tools, frameworks
    /// - person: researchers, engineers, authors
    /// - org: labs, companies, institutions
    #[schemars(description = "concept | document | dataset | project | person | org")]
    pub entity_kind: Option<String>,

    /// Optional description (entity only).
    pub description: Option<String>,

    // ---- note-only fields ----
    /// Text content of the note. Required when kind="note".
    pub content: Option<String>,

    /// Note kind for grouping. One of: observation | insight | question | decision | reference
    /// Defaults to "observation".
    /// Aliases: obs, finding, q, choice, ref, citation
    #[schemars(
        description = "observation | insight | question | decision | reference (default: observation)"
    )]
    pub note_kind: Option<String>,

    /// Salience score between 0.0 (forgettable) and 1.0 (critical). Defaults to 0.5.
    pub salience: Option<f64>,

    /// UUIDs of any substrate (entity, note, edge, or event) this note annotates (creates annotates edges).
    pub annotates: Option<Vec<String>>,

    /// UUID of an existing note this note supersedes. Note only.
    /// Creates a `supersedes` edge from the new note to the old one, and copies
    /// annotation targets from the old note (so the new note annotates the same entities).
    pub supersedes: Option<String>,

    // ---- shared fields ----
    /// Freeform JSON properties.
    pub properties: Option<serde_json::Value>,

    /// Tags for filtering (entity and note).
    pub tags: Option<Vec<String>>,
}
