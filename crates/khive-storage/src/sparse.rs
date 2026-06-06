//! Sparse vector storage and lexical-semantic search capability.

use async_trait::async_trait;
use uuid::Uuid;

use khive_types::SubstrateKind;

use crate::types::{
    BatchWriteSummary, SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector,
    StorageResult,
};

/// Sparse vector storage and similarity search over lexical-semantic signals.
#[async_trait]
pub trait SparseStore: Send + Sync + 'static {
    /// Insert a single sparse vector for a subject.
    async fn insert_sparse(
        &self,
        subject_id: Uuid,
        kind: SubstrateKind,
        namespace: &str,
        field: &str,
        vector: SparseVector,
    ) -> StorageResult<()>;

    /// Insert a batch of sparse vector records.
    async fn insert_batch(&self, records: Vec<SparseRecord>) -> StorageResult<BatchWriteSummary>;

    /// Delete all sparse vectors for the given subject ID.
    async fn delete(&self, subject_id: Uuid) -> StorageResult<bool>;

    /// Run sparse nearest-neighbor search and return ranked hits.
    async fn search_sparse(
        &self,
        request: SparseSearchRequest,
    ) -> StorageResult<Vec<SparseSearchHit>>;

    /// Return the total number of sparse vector entries.
    async fn count(&self) -> StorageResult<u64>;
}
