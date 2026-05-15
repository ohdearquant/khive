//! Vector embedding storage and similarity search capability (ADR-024).

use async_trait::async_trait;
use uuid::Uuid;

use khive_types::SubstrateKind;

use crate::types::{
    BatchWriteSummary, IndexRebuildScope, StorageResult, VectorRecord, VectorSearchHit,
    VectorSearchRequest, VectorStoreInfo,
};

#[async_trait]
pub trait VectorStore: Send + Sync + 'static {
    async fn insert(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        embedding: Vec<f32>,
    ) -> StorageResult<()>;
    async fn insert_batch(&self, records: Vec<VectorRecord>) -> StorageResult<BatchWriteSummary>;
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;
    async fn count(&self) -> StorageResult<u64>;
    async fn search(&self, request: VectorSearchRequest) -> StorageResult<Vec<VectorSearchHit>>;
    async fn info(&self) -> StorageResult<VectorStoreInfo>;
    async fn rebuild(&self, scope: IndexRebuildScope) -> StorageResult<VectorStoreInfo>;
}
