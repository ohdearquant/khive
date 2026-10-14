//! Storage-trait adapters implementing retrieval search traits.
//!
//! `StorageVectorSearch` wraps `Arc<dyn VectorStore>` and `StorageKeywordSearch` wraps
//! `Arc<dyn TextSearch>`, both implementing retrieval traits with `Id = Uuid`.

use std::sync::Arc;

use async_trait::async_trait;
use khive_score::DeterministicScore;
use khive_storage::types::{TextQueryMode, TextSearchRequest, VectorSearchRequest};
use khive_storage::{TextSearch, VectorStore};
use uuid::Uuid;

use crate::error::{Result, RetrievalError};
use crate::hybrid::{KeywordSearch, VectorSearch};

// ---------------------------------------------------------------------------
// Error conversion
// ---------------------------------------------------------------------------

/// Convert a `StorageError` into a `RetrievalError`.
///
/// Maps storage-level errors to the closest retrieval error variant:
/// - Timeout / AdmissionTimeout errors -> `QueryTimeout` (transient, retryable)
/// - InvalidInput errors -> `InvalidQuery` (permanent)
/// - Keyword-context errors -> `Bm25` (permanent)
/// - Everything else -> `Hnsw` (permanent)
pub(super) fn storage_err_to_retrieval(
    err: khive_storage::StorageError,
    context: &'static str,
) -> RetrievalError {
    use khive_storage::StorageError;

    match err {
        // Both timeout shapes are transient. AdmissionTimeout in particular is
        // retryable at the storage layer (the operation never began); dropping
        // it into the permanent Hnsw/Bm25 arm below would erase that signal and
        // turn a saturated-pool retry into a hard failure.
        StorageError::Timeout { .. } | StorageError::AdmissionTimeout { .. } => {
            // Storage timeouts are transient — map to QueryTimeout so retry logic works.
            RetrievalError::QueryTimeout { elapsed_ms: 0 }
        }
        StorageError::InvalidInput { message, .. } => {
            RetrievalError::InvalidQuery(format!("{context}: {message}"))
        }
        other if context.contains("keyword") => RetrievalError::Bm25(format!("{context}: {other}")),
        other => RetrievalError::Hnsw(format!("{context}: {other}")),
    }
}

/// Convert `top_k: usize` to `u32`, returning `InvalidQuery` on overflow.
///
/// `u32::MAX` is 4_294_967_295; any `top_k` larger than that silently wrapped
/// before this fix, turning a huge request into a tiny or zero-result request.
pub(super) fn checked_top_k(top_k: usize) -> crate::error::Result<u32> {
    u32::try_from(top_k)
        .map_err(|_| RetrievalError::InvalidQuery(format!("top_k exceeds u32::MAX: {top_k}")))
}

// ---------------------------------------------------------------------------
// StorageVectorSearch
// ---------------------------------------------------------------------------

/// Adapter implementing [`VectorSearch`] by delegating to a [`VectorStore`].
///
/// Wraps an `Arc<dyn VectorStore>` (e.g., `SqliteVecStore`) and implements
/// the retrieval `VectorSearch` trait with `Id = Uuid`.
///
/// The adapter is `Send + Sync` and can be shared across tasks.
pub struct StorageVectorSearch {
    store: Arc<dyn VectorStore>,
}

impl StorageVectorSearch {
    /// Create a new adapter wrapping the given vector store.
    pub fn new(store: Arc<dyn VectorStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl VectorSearch for StorageVectorSearch {
    type Id = Uuid;

    async fn vector_search(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<(Uuid, DeterministicScore)>> {
        let request = VectorSearchRequest {
            query_vectors: vec![embedding.to_vec()],
            top_k: checked_top_k(top_k)?,
            namespace: None,
            kind: None,
            embedding_model: None,
            filter: None,
            backend_hints: None,
        };

        let hits = self
            .store
            .search(request)
            .await
            .map_err(|e| storage_err_to_retrieval(e, "vector search"))?;

        Ok(hits
            .into_iter()
            .map(|hit| (hit.subject_id, hit.score))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// StorageKeywordSearch
// ---------------------------------------------------------------------------

/// Adapter implementing [`KeywordSearch`] by delegating to a [`TextSearch`].
///
/// Wraps an `Arc<dyn TextSearch>` (e.g., `Fts5TextSearch`) and implements
/// the retrieval `KeywordSearch` trait with `Id = Uuid`.
///
/// Uses `TextQueryMode::Plain` for keyword queries by default. The snippet
/// length is set to 0 since retrieval only needs IDs and scores.
pub struct StorageKeywordSearch {
    search: Arc<dyn TextSearch>,
}

impl StorageKeywordSearch {
    /// Create a new adapter wrapping the given text search backend.
    pub fn new(search: Arc<dyn TextSearch>) -> Self {
        Self { search }
    }
}

#[async_trait]
impl KeywordSearch for StorageKeywordSearch {
    type Id = Uuid;

    async fn keyword_search(
        &self,
        text: &str,
        top_k: usize,
    ) -> Result<Vec<(Uuid, DeterministicScore)>> {
        let request = TextSearchRequest {
            query: text.to_string(),
            mode: TextQueryMode::Plain,
            filter: None,
            top_k: checked_top_k(top_k)?,
            snippet_chars: 0, // retrieval only needs IDs + scores
        };

        let hits = self
            .search
            .search(request)
            .await
            .map_err(|e| storage_err_to_retrieval(e, "keyword search"))?;

        Ok(hits
            .into_iter()
            .map(|hit| (hit.subject_id, hit.score))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "storage/tests.rs"]
mod tests;
