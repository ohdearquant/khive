//! Param/option types for the knowledge pack verbs.

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

// ── atom record (what the SQL stores) ────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct Atom {
    pub id: Uuid,
    pub namespace: String,
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub content: String,
    /// JSON array string e.g. `["rag","retrieval"]`
    pub tags: String,
    /// JSON object string
    pub properties: Option<String>,
    pub finalized: bool,
    pub created_at: i64,
    pub updated_at: i64,
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
    #[allow(dead_code)]
    pub deleted_at: Option<i64>,
}

// ── upsert_atoms ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct AtomInput {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub properties: Option<Value>,
    #[serde(default)]
    pub finalized: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpsertAtomsParams {
    pub atoms: Vec<AtomInput>,
    #[serde(default)]
    #[allow(dead_code)]
    pub chunk_size: Option<usize>,
}

// ── upsert_domains ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
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
}

// ── delete_atoms ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteAtomsParams {
    pub ids: Vec<String>,
    #[serde(default)]
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
}

// ── search ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct SearchParams {
    pub query: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
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
    pub w_description: Option<f64>,
    pub w_tags: Option<f64>,
    pub w_content: Option<f64>,
    pub expand_discount: Option<f64>,
    pub coverage_alpha: Option<f64>,
    pub w_bigram: Option<f64>,
}
