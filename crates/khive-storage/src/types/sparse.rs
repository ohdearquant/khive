//! Sparse vector types for lexical-semantic search.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_types::SubstrateKind;

/// A sparse vector represented as parallel indices and values arrays.
///
/// Invariants: `indices` and `values` must have equal length, `indices` must be
/// strictly increasing, and all `values` must be finite. Use
/// [`validate`](SparseVector::validate) to check.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "SparseVectorRaw")]
pub struct SparseVector {
    /// Dimension indices (must be strictly increasing).
    pub indices: Vec<u32>,
    /// Corresponding non-zero values (must be finite).
    pub values: Vec<f32>,
}

/// Raw deserialization target for [`SparseVector`].
#[derive(Deserialize)]
struct SparseVectorRaw {
    indices: Vec<u32>,
    values: Vec<f32>,
}

impl TryFrom<SparseVectorRaw> for SparseVector {
    type Error = String;

    fn try_from(raw: SparseVectorRaw) -> Result<Self, Self::Error> {
        let sv = Self {
            indices: raw.indices,
            values: raw.values,
        };
        sv.validate()?;
        Ok(sv)
    }
}

impl SparseVector {
    /// Validate: non-empty arrays, equal-length arrays, strictly increasing indices,
    /// all values finite.
    pub fn validate(&self) -> Result<(), String> {
        if self.indices.is_empty() {
            return Err("SparseVector: indices must not be empty".into());
        }
        if self.indices.len() != self.values.len() {
            return Err(format!(
                "SparseVector: indices.len() ({}) != values.len() ({})",
                self.indices.len(),
                self.values.len()
            ));
        }
        for (i, &val) in self.values.iter().enumerate() {
            if !val.is_finite() {
                return Err(format!("SparseVector: values[{i}] is non-finite ({val})"));
            }
        }
        for w in self.indices.windows(2) {
            if w[0] >= w[1] {
                return Err(format!(
                    "SparseVector: indices not strictly increasing at [{}, {}]",
                    w[0], w[1]
                ));
            }
        }
        Ok(())
    }
}

/// A single sparse vector embedding record for bulk insert operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseRecord {
    pub subject_id: Uuid,
    pub kind: SubstrateKind,
    pub namespace: String,
    pub field: String,
    pub vector: SparseVector,
    pub updated_at: DateTime<Utc>,
}

/// Raw deserialization target for [`SparseSearchRequest`].
#[derive(Deserialize)]
struct SparseSearchRequestRaw {
    query: SparseVector,
    top_k: u32,
    namespace: Option<String>,
    kind: Option<SubstrateKind>,
}

impl TryFrom<SparseSearchRequestRaw> for SparseSearchRequest {
    type Error = String;

    fn try_from(raw: SparseSearchRequestRaw) -> Result<Self, Self::Error> {
        if raw.top_k == 0 {
            return Err("SparseSearchRequest: top_k must be > 0".into());
        }
        Ok(Self {
            query: raw.query,
            top_k: raw.top_k,
            namespace: raw.namespace,
            kind: raw.kind,
        })
    }
}

/// Parameters for a sparse nearest-neighbor similarity search. Deserialization rejects top_k = 0.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "SparseSearchRequestRaw")]
pub struct SparseSearchRequest {
    pub query: SparseVector,
    pub top_k: u32,
    pub namespace: Option<String>,
    pub kind: Option<SubstrateKind>,
}

/// A single ranked result from a sparse similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
}
