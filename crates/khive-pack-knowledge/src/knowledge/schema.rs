//! Param/option types for the knowledge pack verbs.

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

pub use khive_brain_core::SectionType;

// ── Section record (what the SQL stores) ─────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct Section {
    pub id: uuid::Uuid,
    pub atom_id: String,
    pub namespace: String,
    pub section_type: SectionType,
    pub heading: String,
    pub content: String,
    pub content_hash: String,
    pub status: String,
    pub tokens: i64,
    pub sort_order: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

// ── atom record (what the SQL stores) ────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct Atom {
    pub id: Uuid,
    pub namespace: String,
    pub slug: String,
    pub name: String,
    /// The atom's content text (also serves as its description). Backed by the
    /// `content` column in `knowledge_atoms`.
    pub content: String,
    /// JSON array string e.g. `["rag","retrieval"]`
    pub tags: String,
    /// JSON object string
    pub properties: Option<String>,
    pub status: Option<String>,
    pub source_uri: Option<String>,
    pub source_type: Option<String>,
    pub finalized: bool,
    pub created_at: i64,
    pub updated_at: i64,
    // REASON: deleted_at is stored in SQL and read when reconstructing full atom history;
    // not yet surfaced to callers but required for soft-delete query correctness.
    #[allow(dead_code)]
    pub deleted_at: Option<i64>,
}

impl Atom {
    /// Comma-separated display of tags (used in FTS scoring text).
    pub fn tags_display(&self) -> String {
        let v: Vec<String> = serde_json::from_str(&self.tags).unwrap_or_default();
        v.join(" ")
    }
}

// ── domain record ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct Domain {
    pub id: Uuid,
    pub namespace: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub tags: String,
    /// JSON array of member atom slugs
    pub members: String,
    pub created_at: i64,
    pub updated_at: i64,
    // REASON: deleted_at is stored in SQL and read when reconstructing full domain history;
    // not yet surfaced to callers but required for soft-delete query correctness.
    #[allow(dead_code)]
    pub deleted_at: Option<i64>,
}

// ── upsert_atoms ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct AtomInput {
    pub slug: String,
    pub name: String,
    /// The atom's content text (>= 20 words).
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub properties: Option<Value>,
    #[serde(default)]
    pub finalized: Option<bool>,
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub source_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpsertAtomsParams {
    pub atoms: Vec<AtomInput>,
    #[serde(default)]
    // REASON: chunk_size is accepted from callers as a hint for client-side pagination;
    // server-side chunking is not yet implemented but the field must be deserialized
    // so callers that send it don't receive unexpected errors.
    #[allow(dead_code)]
    pub chunk_size: Option<usize>,
}

// ── upsert_domains ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DomainInput {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub members: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpsertDomainsParams {
    pub domains: Vec<DomainInput>,
}

// ── get ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct GetParams {
    pub id: String,
    /// When `true`, include the atom's sections in the response under a `sections` key.
    /// Defaults to `false`; domains ignore this flag (they have no sections).
    #[serde(default)]
    pub include_sections: Option<bool>,
}

// ── list ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ListParams {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub exclude_status: Option<String>,
}

// ── delete_atoms ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteAtomsParams {
    pub ids: Vec<String>,
    #[serde(default)]
    // REASON: cascade is accepted from callers for API forward-compatibility; cascading
    // delete behavior is not yet implemented (atoms are soft-deleted without affecting sections).
    #[allow(dead_code)]
    pub cascade: Option<bool>,
}

// ── index ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub(crate) struct IndexParams {
    #[serde(default)]
    pub ids: Option<Vec<String>>,
    #[serde(default)]
    pub batch_size: Option<usize>,
    #[serde(default)]
    pub insert_only: Option<bool>,
    #[serde(default)]
    pub rebuild_ann: Option<bool>,
}

// ── fold ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct FoldParams {
    pub candidates: Vec<FoldCandidate>,
    pub budget: usize,
    #[serde(default)]
    pub min_score: Option<f32>,
    #[serde(default)]
    pub category_weights: Option<std::collections::BTreeMap<String, f32>>,
    #[serde(default)]
    pub diversity_bias: Option<f32>,
    #[serde(default)]
    pub epistemic_weight: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub(crate) struct FoldCandidate {
    pub id: String,
    pub score: f32,
    pub size: usize,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub information_gain: Option<f32>,
}

// ── search ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct SearchParams {
    pub query: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub exclude_status: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub min_score: Option<f64>,
    #[serde(default)]
    pub weights: Option<SearchWeights>,
    #[serde(default)]
    pub decompose: Option<bool>,
    #[serde(default)]
    pub decompose_threshold: Option<usize>,
    #[serde(default)]
    pub intersection_bonus: Option<f64>,
    #[serde(default)]
    pub rerank: Option<bool>,
    #[serde(default)]
    pub rerank_alpha: Option<f64>,
}

/// Tunable TF-IDF weight parameters.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SearchWeights {
    pub w_exact_name: Option<f64>,
    pub w_name: Option<f64>,
    pub w_tags: Option<f64>,
    pub w_content: Option<f64>,
    pub expand_discount: Option<f64>,
    pub coverage_alpha: Option<f64>,
    pub w_bigram: Option<f64>,
}

// ── suggest ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct SuggestParams {
    pub query: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

// ── compose ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ComposeParams {
    #[serde(default)]
    pub domain_ids: Option<Vec<String>>,
    #[serde(default)]
    pub atom_ids: Option<Vec<String>>,
    pub query: String,
    #[serde(default)]
    pub auto_limit: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
}

// ── edit ─────────────────────────────────────────────────────────────────────

/// One section update within a `knowledge.edit` call.
#[derive(Debug, Deserialize)]
pub(crate) struct SectionUpdate {
    /// Section type (must be a valid `SectionType` canonical name).
    pub section_type: String,
    /// New content for this section.  Required.
    pub content: String,
    /// Optional override for the markdown heading text.
    #[serde(default)]
    pub heading: Option<String>,
    /// Sort position within the atom (lower = earlier). Defaults to type ordinal.
    #[serde(default)]
    pub sort_order: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EditParams {
    /// Atom UUID or slug to edit sections for.
    pub id: String,
    /// Sections to upsert.  Must not be empty.
    pub sections: Vec<SectionUpdate>,
}

// ── challenge / adjudicate ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ChallengeParams {
    /// Atom UUID or slug.
    pub atom_id: String,
    /// Section type to challenge.
    pub section_type: String,
    /// Disambiguator when an atom has multiple same-type sections
    /// (`content_hash` from `knowledge.edit`). Required if more than one
    /// eligible section of `section_type` exists.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// Optional challenge reason.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AdjudicateParams {
    /// Atom UUID or slug.
    pub atom_id: String,
    /// Section type to adjudicate.
    pub section_type: String,
    /// Disambiguator when an atom has multiple same-type disputed sections
    /// (`content_hash` from `knowledge.edit`). Required if more than one
    /// disputed section of `section_type` exists.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// Resolution: "accept" (keep disputed, mark reviewed) or "reject" (revert to reviewed).
    pub resolution: String,
}

// ── import ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ImportParams {
    /// Filesystem path to a markdown file or directory.
    pub path: String,
    /// Markdown format hint.  Only `"atlas_md"` is supported in v1.
    #[serde(default)]
    pub format: Option<String>,
    /// Chunk strategy: `"section"` (one section per atom, default) or `"atom"`
    /// (entire file as one atom).
    #[serde(default)]
    pub chunk_strategy: Option<String>,
}
