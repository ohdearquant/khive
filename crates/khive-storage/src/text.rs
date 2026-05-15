//! Full-text search capability (ADR-024).

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, IndexRebuildScope, StorageResult, TextDocument, TextFilter, TextIndexStats,
    TextSearchHit, TextSearchRequest,
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
}
