//! Full-text search capability.

use async_trait::async_trait;
use uuid::Uuid;

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::{
    BatchWriteSummary, IndexRebuildScope, StorageResult, TextDocument, TextFilter, TextIndexStats,
    TextSearchHit, TextSearchOptions, TextSearchRequest, TextTermStats, TextTermStatsRequest,
};

/// Full-text search capability over indexed documents.
#[async_trait]
pub trait TextSearch: Send + Sync + 'static {
    /// Index or update a single text document.
    async fn upsert_document(&self, document: TextDocument) -> StorageResult<()>;
    /// Index or update a batch of text documents.
    async fn upsert_documents(
        &self,
        documents: Vec<TextDocument>,
    ) -> StorageResult<BatchWriteSummary>;
    /// Remove a document from the text index.
    async fn delete_document(&self, namespace: &str, subject_id: Uuid) -> StorageResult<bool>;
    /// Fetch an indexed document by namespace and subject ID.
    async fn get_document(
        &self,
        namespace: &str,
        subject_id: Uuid,
    ) -> StorageResult<Option<TextDocument>>;
    /// Run a full-text search query and return ranked hits.
    async fn search(&self, request: TextSearchRequest) -> StorageResult<Vec<TextSearchHit>>;
    /// Count documents matching a filter.
    async fn count(&self, filter: TextFilter) -> StorageResult<u64>;
    /// Return index metadata and health statistics.
    async fn stats(&self) -> StorageResult<TextIndexStats>;
    /// Rebuild the text index, optionally scoped to a subset of entries.
    async fn rebuild(&self, scope: IndexRebuildScope) -> StorageResult<TextIndexStats>;

    /// Search with explicit gather options (candidate-gather optimization).
    ///
    /// Default delegates to `search` when options are default (Ranked mode).
    /// Backends that do not implement this return `StorageError::Unsupported`
    /// for non-default options, which the caller must handle by falling back
    /// or propagating.
    async fn search_with_options(
        &self,
        request: TextSearchRequest,
        options: TextSearchOptions,
    ) -> StorageResult<Vec<TextSearchHit>> {
        if options == TextSearchOptions::default() {
            self.search(request).await
        } else {
            Err(StorageError::Unsupported {
                capability: StorageCapability::Text,
                operation: "search_with_options".into(),
                message: "this backend does not implement non-default gather options".into(),
            })
        }
    }

    /// Return per-term document frequency and IDF for a set of query terms.
    ///
    /// Default returns `StorageError::Unsupported`. Only FTS5-backed stores
    /// provide a concrete implementation.
    async fn term_stats(
        &self,
        _request: TextTermStatsRequest,
    ) -> StorageResult<Vec<TextTermStats>> {
        Err(StorageError::Unsupported {
            capability: StorageCapability::Text,
            operation: "term_stats".into(),
            message: "this backend does not implement term_stats".into(),
        })
    }
}
