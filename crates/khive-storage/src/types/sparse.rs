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

/// Upper bound on `SparseSearchRequest::top_k` to prevent request-sized heap
/// allocation in first-party backends (STORAGE-AUD-002).
pub const MAX_SPARSE_SEARCH_TOP_K: u32 = 10_000;

impl TryFrom<SparseSearchRequestRaw> for SparseSearchRequest {
    type Error = String;

    fn try_from(raw: SparseSearchRequestRaw) -> Result<Self, Self::Error> {
        let request = Self {
            query: raw.query,
            top_k: raw.top_k,
            namespace: raw.namespace,
            kind: raw.kind,
        };
        request.validate()?;
        Ok(request)
    }
}

/// Parameters for a sparse nearest-neighbor similarity search. Deserialization
/// rejects top_k = 0 and top_k > [`MAX_SPARSE_SEARCH_TOP_K`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "SparseSearchRequestRaw")]
pub struct SparseSearchRequest {
    pub query: SparseVector,
    pub top_k: u32,
    pub namespace: Option<String>,
    pub kind: Option<SubstrateKind>,
}

impl SparseSearchRequest {
    /// Validate `top_k` bounds. Backends must call this even when the request
    /// is constructed directly (bypassing serde), since the fields are public.
    pub fn validate(&self) -> Result<(), String> {
        if self.top_k == 0 {
            return Err("SparseSearchRequest: top_k must be > 0".into());
        }
        if self.top_k > MAX_SPARSE_SEARCH_TOP_K {
            return Err(format!(
                "SparseSearchRequest: top_k must be <= {MAX_SPARSE_SEARCH_TOP_K}, got {}",
                self.top_k
            ));
        }
        Ok(())
    }
}

/// A single ranked result from a sparse similarity search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseSearchHit {
    pub subject_id: Uuid,
    pub score: khive_score::DeterministicScore,
    pub rank: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_query() -> SparseVector {
        SparseVector {
            indices: vec![0],
            values: vec![1.0],
        }
    }

    /// STORAGE-AUD-002 / #470: top_k = u32::MAX must be rejected by serde
    /// deserialization instead of reaching a backend that would allocate a
    /// multi-hundred-GB heap.
    #[test]
    fn sparse_top_k_u32_max_rejected() {
        let raw = serde_json::json!({
            "query": {"indices": [0], "values": [1.0]},
            "top_k": u32::MAX,
            "namespace": null,
            "kind": null,
        });
        let result: Result<SparseSearchRequest, _> = serde_json::from_value(raw);
        assert!(
            result.is_err(),
            "top_k = u32::MAX must be rejected, got {result:?}"
        );
    }

    #[test]
    fn sparse_top_k_at_max_accepted() {
        let request = SparseSearchRequest {
            query: sample_query(),
            top_k: MAX_SPARSE_SEARCH_TOP_K,
            namespace: None,
            kind: None,
        };
        assert!(request.validate().is_ok());
    }

    #[test]
    fn sparse_top_k_over_max_rejected_direct_construction() {
        let request = SparseSearchRequest {
            query: sample_query(),
            top_k: MAX_SPARSE_SEARCH_TOP_K + 1,
            namespace: None,
            kind: None,
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn sparse_top_k_zero_still_rejected() {
        let request = SparseSearchRequest {
            query: sample_query(),
            top_k: 0,
            namespace: None,
            kind: None,
        };
        assert!(request.validate().is_err());
    }
}
