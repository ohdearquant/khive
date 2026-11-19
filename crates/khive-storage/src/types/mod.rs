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
    DirectedNeighborHit, Direction, Edge, EdgeEndpointBaseCounts, EdgeFilter, EdgeSeekPage,
    EdgeSortField, EdgeUpsertDisposition, EdgeUpsertRefusal, EdgeUpsertRequest, EdgeUpsertResult,
    GraphPath, GuardedBatchOutcome, GuardedBatchRefusal, GuardedEdgeBatchRefusal,
    GuardedEdgeBatchUpsertOutcome, GuardedEdgeUpsertOutcome, GuardedWriteOutcome, LinkId,
    MissingEndpoints, NeighborCursor, NeighborHit, NeighborQuery, PathNode, SortDirection,
    SortOrder, TimeRange, TraversalExecutionBudget, TraversalOptions, TraversalRequest,
    DEFAULT_TRAVERSAL_LIMIT, MAX_TRAVERSAL_DEPTH, MAX_TRAVERSAL_LIMIT, MAX_TRAVERSAL_MILLIS,
    MAX_TRAVERSAL_ROOTS, MAX_TRAVERSAL_WORK,
};
pub use pagination::{BoundedCount, LimitReport, Page, PageRequest, SeekCursor, SeekPage};
pub use sparse::{
    SparseRecord, SparseSearchHit, SparseSearchRequest, SparseVector, MAX_SPARSE_SEARCH_TOP_K,
};
pub use sql::{SqlColumn, SqlColumnError, SqlRow, SqlStatement, SqlValue};
pub use text::{
    IndexRebuildScope, TextDocument, TextFilter, TextGatherMode, TextIndexStats, TextQueryMode,
    TextSearchHit, TextSearchOptions, TextSearchRequest, TextTermStats, TextTermStatsRequest,
};
pub use vector::{
    OrphanSweepConfig, OrphanSweepResult, PropertyFilter, PropertyOp, VectorIndexKind,
    VectorMetadataFilter, VectorProvenance, VectorRecord, VectorScanEntry, VectorScanPage,
    VectorScanRequest, VectorSearchHit, VectorSearchRequest, VectorStoreCapabilities,
    VectorStoreInfo,
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
        self.record_failure_with(index, class, retryability, || (item_id, message.into()));
    }

    /// Record a refusal without constructing an item identity or message that
    /// will be omitted. The closure runs only for a sampled detail or when the
    /// legacy `first_error` still needs to be populated.
    pub fn record_failure_with(
        &mut self,
        index: usize,
        class: BatchWriteErrorClass,
        retryability: BatchWriteRetryability,
        detail: impl FnOnce() -> (Option<String>, String),
    ) {
        self.failed = self.failed.saturating_add(1);

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

        let sampled = self.errors.len() < MAX_BATCH_WRITE_ERROR_DETAILS;
        let detail = (sampled || self.first_error.is_empty()).then(detail);
        if let Some((_, message)) = &detail {
            if self.first_error.is_empty() {
                self.first_error = message.clone();
            }
        }
        if let Some((item_id, message)) = detail.filter(|_| sampled) {
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

impl GuardedBatchRefusal {
    fn error_metadata(&self, index: usize) -> (BatchWriteErrorClass, BatchWriteRetryability) {
        if index == self.entry_index {
            (
                BatchWriteErrorClass::InvalidInput,
                BatchWriteRetryability::Permanent,
            )
        } else {
            (
                BatchWriteErrorClass::BatchAborted,
                BatchWriteRetryability::Unknown,
            )
        }
    }

    fn format_failure(
        &self,
        index: usize,
        edge: &Edge,
        first_error: &str,
    ) -> (Option<String>, String) {
        let message = if index == self.entry_index {
            first_error.to_owned()
        } else {
            format!(
                "batch entry {index} was not written because guarded batch entry {} was refused",
                self.entry_index,
            )
        };
        (Some(edge.id.to_string()), message)
    }

    /// Record one original batch entry using the same classification and
    /// diagnostic as [`GuardedBatchOutcome::refusal_page`]. Siblings are aborted,
    /// not independently diagnosed as having missing endpoints.
    pub fn record_failure(
        &self,
        summary: &mut BatchWriteSummary,
        index: usize,
        edge: &Edge,
        first_error: &str,
    ) {
        let (class, retryability) = self.error_metadata(index);
        summary.record_failure_with(index, class, retryability, || {
            self.format_failure(index, edge, first_error)
        });
    }
}

impl GuardedBatchOutcome {
    /// Enumerate the refused writes from the caller-retained original batch,
    /// without resubmitting or rechecking endpoints. Retain the exact original
    /// edges and order: only their length and the refusal index can be validated.
    ///
    /// The class filter applies before offset/limit. Every page is capped at
    /// [`MAX_BATCH_WRITE_ERROR_DETAILS`]; zero limit returns only the matching
    /// total. Continue by adding the returned item count to `page.offset`.
    /// Success and offsets beyond the matching population return empty pages.
    /// This is an in-memory storage API, not a new runtime/MCP endpoint.
    pub fn refusal_page(
        &self,
        original_edges: &[Edge],
        class: Option<BatchWriteErrorClass>,
        page: PageRequest,
    ) -> StorageResult<Page<BatchWriteError>> {
        self.refusal_page_with(
            original_edges,
            class,
            page,
            GuardedBatchRefusal::format_failure,
        )
    }

    fn refusal_page_with(
        &self,
        original_edges: &[Edge],
        class: Option<BatchWriteErrorClass>,
        page: PageRequest,
        mut format: impl FnMut(&GuardedBatchRefusal, usize, &Edge, &str) -> (Option<String>, String),
    ) -> StorageResult<Page<BatchWriteError>> {
        let invalid = |message| StorageError::InvalidInput {
            capability: crate::capability::StorageCapability::Graph,
            operation: "guarded_batch_refusal_page".into(),
            message,
        };
        let len = u64::try_from(original_edges.len()).unwrap_or(u64::MAX);
        if len != self.summary.attempted {
            return Err(invalid(format!(
                "original batch length {len} does not match attempted count {}",
                self.summary.attempted,
            )));
        }
        let Some(refusal) = &self.refused else {
            return Ok(Page {
                items: Vec::new(),
                total: Some(0),
            });
        };
        if refusal.entry_index >= original_edges.len() {
            return Err(invalid(format!(
                "refusal index {} is outside original batch length {len}",
                refusal.entry_index,
            )));
        }
        let matching = (0..original_edges.len())
            .filter(|index| class.is_none_or(|class| refusal.error_metadata(*index).0 == class));
        let total = matching.clone().count() as u64;
        let offset = usize::try_from(page.offset).unwrap_or(usize::MAX);
        let limit = (page.limit as usize).min(MAX_BATCH_WRITE_ERROR_DETAILS);
        let items = matching
            .skip(offset)
            .take(limit)
            .map(|index| {
                let (class, retryability) = refusal.error_metadata(index);
                let (item_id, message) = format(
                    refusal,
                    index,
                    &original_edges[index],
                    &self.summary.first_error,
                );
                BatchWriteError {
                    index: index as u64,
                    item_id,
                    class,
                    retryability,
                    message: bounded_batch_error_message(&message),
                }
            })
            .collect();
        Ok(Page {
            items,
            total: Some(total),
        })
    }
}

#[cfg(test)]
#[path = "batch_write_summary_tests.rs"]
mod batch_write_summary_tests;

/// Controls whether a delete operation removes the record immediately or marks it as deleted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    /// Mark `deleted_at`; record remains queryable with explicit soft-delete filter.
    Soft,
    /// Physically remove the row and cascade incident edges.
    Hard,
}
