//! Full-text search capability (ADR-024).

use async_trait::async_trait;
use uuid::Uuid;

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::{
    BatchWriteSummary, IndexRebuildScope, StorageResult, TextDocument, TextFilter, TextIndexStats,
    TextSearchHit, TextSearchOptions, TextSearchRequest, TextTermStats, TextTermStatsRequest,
};

#[async_trait]
pub trait TextSearch: Send + Sync + 'static {
    async fn upsert_document(&self, document: TextDocument) -> StorageResult<()>;
    async fn upsert_documents(
        &self,
        documents: Vec<TextDocument>,
    ) -> StorageResult<BatchWriteSummary>;
    async fn delete_document(&self, namespace: &str, subject_id: Uuid) -> StorageResult<bool>;
    async fn get_document(
        &self,
        namespace: &str,
        subject_id: Uuid,
    ) -> StorageResult<Option<TextDocument>>;
    async fn search(&self, request: TextSearchRequest) -> StorageResult<Vec<TextSearchHit>>;
    async fn count(&self, filter: TextFilter) -> StorageResult<u64>;
    async fn stats(&self) -> StorageResult<TextIndexStats>;
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
