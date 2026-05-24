//! Sparse vector storage and lexical-semantic search capability (ADR-031).

use async_trait::async_trait;
use uuid::Uuid;

use crate::types::{
    BatchWriteSummary, SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector,
    StorageResult,
};

#[async_trait]
pub trait SparseStore: Send + Sync + 'static {
    async fn insert_sparse(
        &self,
        namespace: &str,
        subject_id: Uuid,
        field: &str,
        vector: SparseVector,
    ) -> StorageResult<()>;

    async fn insert_batch(
        &self,
        records: Vec<SparseRecord>,
    ) -> StorageResult<BatchWriteSummary>;

    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;

    async fn search_sparse(
        &self,
        request: SparseSearchRequest,
    ) -> StorageResult<Vec<SparseSearchHit>>;

    async fn count(&self) -> StorageResult<u64>;
}
