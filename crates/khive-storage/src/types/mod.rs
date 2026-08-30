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
    DirectedNeighborHit, Direction, Edge, EdgeFilter, EdgeSeekPage, EdgeSortField, GraphPath,
    GuardedBatchOutcome, GuardedBatchRefusal, GuardedWriteOutcome, LinkId, MissingEndpoints,
    NeighborHit, NeighborQuery, PathNode, SortDirection, SortOrder, TimeRange,
    TraversalExecutionBudget, TraversalOptions, TraversalRequest, DEFAULT_TRAVERSAL_LIMIT,
    MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_LIMIT, MAX_TRAVERSAL_MILLIS, MAX_TRAVERSAL_ROOTS,
    MAX_TRAVERSAL_WORK,
};
pub use pagination::{Page, PageRequest, SeekCursor, SeekPage};
pub use sparse::{
    SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector, MAX_SPARSE_SEARCH_TOP_K,
};
pub use sql::{SqlColumn, SqlRow, SqlStatement, SqlValue};
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

/// Maximum number of per-item refusal details returned by one batch write.
/// Class counts continue across the complete batch after this sample fills.
pub const MAX_BATCH_WRITE_ERROR_DETAILS: usize = 128;

/// Maximum number of Unicode scalar values retained in a sampled refusal
/// message. The legacy `first_error` field remains byte-for-byte compatible.
pub const MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS: usize = 512;

/// Stable reason class for one item refused by a best-effort batch write.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchWriteErrorClass {
    InvalidInput,
    Constraint,
    Conflict,
    /// The item was not attempted because another item caused an atomic
    /// batch refusal.
    BatchAborted,
    Serialization,
    Busy,
    Cancelled,
    Driver,
    Unknown,
}

/// Whether retrying the exact refused item is expected to be useful.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchWriteRetryability {
    Permanent,
    Transient,
    Unknown,
}

/// Bounded detail for one refused input item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchWriteError {
    /// Zero-based position in the submitted batch.
    pub index: u64,
    /// Stable store identity when the input type has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub class: BatchWriteErrorClass,
    pub retryability: BatchWriteRetryability,
    pub message: String,
}

/// Complete count for one `(class, retryability)` partition, including
/// details omitted after the bounded sample fills.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatchWriteErrorCount {
    pub class: BatchWriteErrorClass,
    pub retryability: BatchWriteRetryability,
    pub count: u64,
}

/// Aggregate outcome of a batch write operation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BatchWriteSummary {
    pub attempted: u64,
    pub affected: u64,
    pub failed: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub first_error: String,
    /// Bounded per-item refusal details in input order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<BatchWriteError>,
    /// Complete refusal counts even when `errors` reaches its cap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_counts: Vec<BatchWriteErrorCount>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub errors_truncated: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub errors_omitted: u64,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl BatchWriteSummary {
    /// Record one best-effort item refusal while retaining the legacy
    /// aggregate and first-error fields.
    pub fn record_failure(
        &mut self,
        index: usize,
        item_id: Option<String>,
        class: BatchWriteErrorClass,
        retryability: BatchWriteRetryability,
        message: impl Into<String>,
    ) {
        let message = message.into();
        self.failed = self.failed.saturating_add(1);
        if self.first_error.is_empty() {
            self.first_error = message.clone();
        }

        match self
            .error_counts
            .iter_mut()
            .find(|count| count.class == class && count.retryability == retryability)
        {
            Some(count) => count.count = count.count.saturating_add(1),
            None => {
                self.error_counts.push(BatchWriteErrorCount {
                    class,
                    retryability,
                    count: 1,
                });
                self.error_counts
                    .sort_by_key(|count| (count.class, count.retryability));
            }
        }

        if self.errors.len() < MAX_BATCH_WRITE_ERROR_DETAILS {
            self.errors.push(BatchWriteError {
                index: u64::try_from(index).unwrap_or(u64::MAX),
                item_id,
                class,
                retryability,
                message: bounded_batch_error_message(&message),
            });
        } else {
            self.errors_truncated = true;
            self.errors_omitted = self.errors_omitted.saturating_add(1);
        }
    }
}

fn bounded_batch_error_message(message: &str) -> String {
    if message.chars().count() <= MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS {
        return message.to_owned();
    }

    let mut bounded: String = message
        .chars()
        .take(MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS.saturating_sub(1))
        .collect();
    bounded.push('\u{2026}');
    bounded
}

#[cfg(test)]
mod batch_write_summary_tests {
    use super::*;

    #[test]
    fn refusal_sample_is_bounded_but_counts_cover_the_complete_batch() {
        let total = MAX_BATCH_WRITE_ERROR_DETAILS + 3;
        let mut summary = BatchWriteSummary {
            attempted: total as u64,
            ..BatchWriteSummary::default()
        };

        for index in 0..total {
            summary.record_failure(
                index,
                Some(format!("item-{index}")),
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
                "invalid item",
            );
        }

        assert_eq!(summary.failed, total as u64);
        assert_eq!(summary.errors.len(), MAX_BATCH_WRITE_ERROR_DETAILS);
        assert!(summary.errors_truncated);
        assert_eq!(summary.errors_omitted, 3);
        assert_eq!(summary.error_counts.len(), 1);
        assert_eq!(summary.error_counts[0].count, total as u64);
        assert_eq!(
            summary.error_counts[0].class,
            BatchWriteErrorClass::InvalidInput
        );
        assert_eq!(
            summary.error_counts[0].retryability,
            BatchWriteRetryability::Permanent
        );
        assert_eq!(
            summary
                .error_counts
                .iter()
                .map(|count| count.count)
                .sum::<u64>(),
            summary.failed
        );
    }

    #[test]
    fn sampled_message_is_bounded_without_changing_legacy_first_error() {
        let message = "x".repeat(MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS + 10);
        let mut summary = BatchWriteSummary::default();

        summary.record_failure(
            0,
            None,
            BatchWriteErrorClass::Driver,
            BatchWriteRetryability::Unknown,
            message.clone(),
        );

        assert_eq!(summary.first_error, message);
        assert_eq!(
            summary.errors[0].message.chars().count(),
            MAX_BATCH_WRITE_ERROR_MESSAGE_CHARS
        );
        assert!(summary.errors[0].message.ends_with('\u{2026}'));
    }

    #[test]
    fn successful_summary_keeps_the_legacy_wire_shape() {
        let summary = BatchWriteSummary {
            attempted: 2,
            affected: 2,
            ..BatchWriteSummary::default()
        };

        let value = serde_json::to_value(summary).expect("serialize summary");
        assert_eq!(
            value,
            serde_json::json!({"attempted": 2, "affected": 2, "failed": 0})
        );
    }

    #[test]
    fn legacy_wire_shape_deserializes_with_empty_refusal_details() {
        let summary: BatchWriteSummary = serde_json::from_value(serde_json::json!({
            "attempted": 4,
            "affected": 3,
            "failed": 1,
            "first_error": "legacy"
        }))
        .expect("deserialize legacy summary");

        assert_eq!(summary.attempted, 4);
        assert_eq!(summary.affected, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.first_error, "legacy");
        assert!(summary.errors.is_empty());
        assert!(summary.error_counts.is_empty());
        assert!(!summary.errors_truncated);
        assert_eq!(summary.errors_omitted, 0);
    }
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
