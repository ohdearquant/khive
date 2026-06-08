//! Dense vector storage and similarity search types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_types::SubstrateKind;

/// Discriminant for the ANN index algorithm used by a vector backend.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VectorIndexKind {
    Hnsw,
    SqliteVec,
    Flat,
}

/// Backend capability declaration for vector stores.
///
/// Returned by [`crate::VectorStore::capabilities`]. Higher-level retrieval
/// policy introspects this struct at construction time to select the optimal
/// code path without relying on error-type matching.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreCapabilities {
    /// Supports metadata pre-filter pushdown into the index scan.
    pub supports_filter: bool,
    /// Supports batch search (multiple query vectors in one call).
    pub supports_batch_search: bool,
    /// Supports quantization (reduces memory; may trade recall).
    pub supports_quantization: bool,
    /// Supports in-place update without a delete+insert round-trip.
    pub supports_update: bool,
    /// Supports orphan sweep (deleting vectors with no live subject).
    pub supports_orphan_sweep: bool,
    /// Supports multiple named fields per subject (e.g. `entity.title` and
    /// `entity.body` stored as separate vectors). sqlite-vec backends use a
    /// `subject_id PRIMARY KEY` table and therefore only support one vector
    /// per subject per namespace -- this field is `false` for those backends.
    #[serde(default)]
    pub supports_multi_field: bool,
    /// Maximum supported embedding dimension, or `None` if unbounded.
    pub max_dimensions: Option<u32>,
    /// Index algorithms available in this backend.
    pub index_kinds: Vec<VectorIndexKind>,
}

/// Comparison operators for [`PropertyFilter`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PropertyOp {
    Eq,
    Ne,
    In,
    Range,
    Exists,
}

/// A single typed metadata predicate used in [`VectorMetadataFilter`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PropertyFilter {
    pub key: String,
    pub op: PropertyOp,
    pub value: serde_json::Value,
}

/// A typed predicate for backend-pushable metadata filtering.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VectorMetadataFilter {
    /// Restrict to these namespaces.
    pub namespaces: Vec<String>,
    /// Restrict to these substrate kinds.
    pub kinds: Vec<SubstrateKind>,
    /// Typed property predicates.
    pub property_filters: Vec<PropertyFilter>,
}

impl VectorMetadataFilter {
    /// Returns `true` when no predicates are set (filter is a no-op).
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty() && self.kinds.is_empty() && self.property_filters.is_empty()
    }
}

/// A single vector embedding record for bulk insert operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorRecord {
    pub subject_id: Uuid,
    pub kind: SubstrateKind,
    pub namespace: String,
    /// Which embedding field this record represents (e.g. `"entity.body"`).
    pub field: String,
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// One or many dense vectors; sqlite-vec backends enforce `vectors.len() == 1`.
    pub vectors: Vec<Vec<f32>>,
    pub updated_at: DateTime<Utc>,
}

/// Parameters for a nearest-neighbor similarity search.
///
/// Use [`validate`](VectorSearchRequest::validate) to check invariants before
/// passing to a backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "VectorSearchRequestRaw")]
pub struct VectorSearchRequest {
    /// One or many query vectors; sqlite-vec backends enforce `query_vectors.len() == 1`.
    pub query_vectors: Vec<Vec<f32>>,
    pub top_k: u32,
    pub namespace: Option<String>,
    pub kind: Option<SubstrateKind>,
    /// Restrict results to this embedding model. Defaults to the store's own model.
    #[serde(default)]
    pub embedding_model: Option<String>,
    /// Optional metadata filter for backends that support pushdown.
    pub filter: Option<VectorMetadataFilter>,
    /// Backend-specific hints (opaque JSON blob, ignored by default).
    pub backend_hints: Option<serde_json::Value>,
}

/// Raw deserialization target for [`VectorSearchRequest`].
#[derive(Deserialize)]
struct VectorSearchRequestRaw {
    query_vectors: Vec<Vec<f32>>,
    top_k: u32,
    namespace: Option<String>,
    kind: Option<SubstrateKind>,
    #[serde(default)]
    embedding_model: Option<String>,
    filter: Option<VectorMetadataFilter>,
    backend_hints: Option<serde_json::Value>,
}

impl TryFrom<VectorSearchRequestRaw> for VectorSearchRequest {
    type Error = String;

    fn try_from(raw: VectorSearchRequestRaw) -> Result<Self, Self::Error> {
        let req = Self {
            query_vectors: raw.query_vectors,
            top_k: raw.top_k,
            namespace: raw.namespace,
            kind: raw.kind,
            embedding_model: raw.embedding_model,
            filter: raw.filter,
            backend_hints: raw.backend_hints,
        };
        req.validate()?;
        Ok(req)
    }
}

impl VectorSearchRequest {
    /// Validate: non-empty query vectors, each inner vector non-empty, finite values,
    /// non-zero `top_k`. Returns first violation.
    pub fn validate(&self) -> Result<(), String> {
        if self.query_vectors.is_empty() {
            return Err("VectorSearchRequest: query_vectors must not be empty".into());
        }
        if self.top_k == 0 {
            return Err("VectorSearchRequest: top_k must be > 0".into());
        }
        for (qi, qvec) in self.query_vectors.iter().enumerate() {
            if qvec.is_empty() {
                return Err(format!(
                    "VectorSearchRequest: query_vectors[{qi}] must not be empty"
                ));
            }
            for (vi, &v) in qvec.iter().enumerate() {
                if !v.is_finite() {
                    return Err(format!(
                        "VectorSearchRequest: query_vectors[{qi}][{vi}] is non-finite ({v})"
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Configuration for a vector orphan-sweep pass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrphanSweepConfig {
    /// Optional allowlist of subject IDs to check. `None` = scan all rows.
    /// `Some(ids)` restricts the sweep to only those IDs; rows not in the list
    /// are untouched even if orphaned.
    pub subject_id_allowlist: Option<Vec<Uuid>>,
    pub namespaces: Vec<String>,
    pub substrate_kinds: Vec<SubstrateKind>,
    pub max_delete: u32,
    pub dry_run: bool,
}

/// Result of a vector orphan-sweep pass.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrphanSweepResult {
    pub scanned: u64,
    pub deleted: u64,
    pub would_delete: u64,
    pub max_delete_hit: bool,
}

/// A single ranked result from a dense vector similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
}

/// Metadata and health summary for a vector index backend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorStoreInfo {
    pub model_name: String,
    pub dimensions: usize,
    pub index_kind: VectorIndexKind,
    pub entry_count: u64,
    pub needs_rebuild: bool,
    pub last_rebuild_at: Option<DateTime<Utc>>,
}
