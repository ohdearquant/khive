//! Shared types used across storage capability traits.

mod graph;
mod pagination;
mod sparse;
mod sql;
mod text;
mod vector;

use crate::error::StorageError;

/// Convenience alias for `Result<T, StorageError>` used throughout this crate.
pub type StorageResult<T> = Result<T, StorageError>;

pub use graph::{
    Direction, Edge, EdgeFilter, EdgeSortField, GraphPath, LinkId, NeighborHit, NeighborQuery,
    PathNode, SortDirection, SortOrder, TimeRange, TraversalOptions, TraversalRequest,
};
pub use pagination::{Page, PageRequest};
pub use sparse::{SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector};
pub use sql::{SqlColumn, SqlIsolation, SqlRow, SqlStatement, SqlTxOptions, SqlValue};
pub use text::{
    IndexRebuildScope, TextDocument, TextFilter, TextGatherMode, TextIndexStats, TextQueryMode,
    TextSearchHit, TextSearchOptions, TextSearchRequest, TextTermStats, TextTermStatsRequest,
};
pub use vector::{
    OrphanSweepConfig, OrphanSweepResult, PropertyFilter, PropertyOp, VectorIndexKind,
    VectorMetadataFilter, VectorRecord, VectorSearchHit, VectorSearchRequest,
    VectorStoreCapabilities, VectorStoreInfo,
};

use serde::{Deserialize, Serialize};

/// Aggregate outcome of a batch write operation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BatchWriteSummary {
    pub attempted: u64,
    pub affected: u64,
    pub failed: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_error: String,
}

/// Controls whether a delete operation removes the record immediately or marks it as deleted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    /// Mark `deleted_at`; record remains queryable with explicit soft-delete filter.
    Soft,
    /// Physically remove the row and cascade incident edges.
    Hard,
}
