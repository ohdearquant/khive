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
pub use pagination::{BoundedCount, Page, PageRequest, SeekCursor, SeekPage};
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
mod batch_write_summary_tests {
    use super::*;

    fn guarded_refusal_fixture(
        len: usize,
        refused_index: usize,
    ) -> (Vec<Edge>, GuardedBatchOutcome) {
        let edges: Vec<_> = (0..len)
            .map(|index| Edge {
                id: LinkId(uuid::Uuid::from_u128(index as u128 + 1)),
                namespace: "local".into(),
                source_id: uuid::Uuid::from_u128(1001),
                target_id: uuid::Uuid::from_u128(1002),
                relation: khive_types::EdgeRelation::Extends,
                weight: 1.0,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                deleted_at: None,
                metadata: None,
                target_backend: None,
            })
            .collect();
        let refusal = GuardedBatchRefusal {
            entry_index: refused_index,
            missing: MissingEndpoints {
                source: false,
                target: true,
            },
        };
        let mut summary = BatchWriteSummary {
            attempted: len as u64,
            first_error: "original guard refusal".into(),
            ..Default::default()
        };
        for (index, edge) in edges.iter().enumerate() {
            refusal.record_failure(&mut summary, index, edge, "original guard refusal");
        }
        (
            edges,
            GuardedBatchOutcome {
                summary,
                refused: Some(refusal),
            },
        )
    }

    #[test]
    fn guarded_refusal_page_formats_only_filtered_page_with_shared_cap() {
        let cap = MAX_BATCH_WRITE_ERROR_DETAILS;
        let (edges, outcome) = guarded_refusal_fixture(cap * 2 + 9, cap + 2);
        for (class, offset, expected_len, expected_total, expected_first) in [
            (None, cap as u64, cap, edges.len(), Some(cap)),
            (
                Some(BatchWriteErrorClass::InvalidInput),
                0,
                1,
                1,
                Some(cap + 2),
            ),
            (
                Some(BatchWriteErrorClass::BatchAborted),
                (cap + 3) as u64,
                cap,
                edges.len() - 1,
                Some(cap + 4),
            ),
            (Some(BatchWriteErrorClass::Conflict), 0, 0, 0, None),
        ] {
            let mut formatted = Vec::new();
            let page = outcome
                .refusal_page_with(
                    &edges,
                    class,
                    PageRequest {
                        offset,
                        limit: u32::MAX,
                    },
                    |refusal, index, edge, message| {
                        formatted.push(index);
                        refusal.format_failure(index, edge, message)
                    },
                )
                .unwrap();
            assert_eq!(page.items.len(), expected_len);
            assert_eq!(formatted.len(), expected_len);
            assert_eq!(formatted.first().copied(), expected_first);
            assert_eq!(page.total, Some(expected_total as u64));
            assert_eq!(
                formatted,
                page.items
                    .iter()
                    .map(|error| error.index as usize)
                    .collect::<Vec<_>>()
            );
            if let Some(class) = class {
                assert!(page.items.iter().all(|error| error.class == class));
            }
        }
    }

    #[test]
    fn guarded_refusal_page_handles_empty_success_and_page_boundaries() {
        let (edges, outcome) = guarded_refusal_fixture(3, 1);
        for page in [
            PageRequest {
                offset: 0,
                limit: 0,
            },
            PageRequest {
                offset: 3,
                limit: 1,
            },
            PageRequest {
                offset: u64::MAX,
                limit: u32::MAX,
            },
        ] {
            let result = outcome
                .refusal_page_with(&edges, None, page, |_, _, _, _| {
                    panic!("empty page must not format a detail")
                })
                .unwrap();
            assert!(result.items.is_empty());
            assert_eq!(result.total, Some(3));
        }
        let success = GuardedBatchOutcome {
            summary: BatchWriteSummary {
                attempted: 3,
                affected: 3,
                ..Default::default()
            },
            refused: None,
        };
        let page = success
            .refusal_page(&edges, None, PageRequest::default())
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, Some(0));
        let empty = GuardedBatchOutcome {
            summary: BatchWriteSummary::default(),
            refused: None,
        };
        let page = empty
            .refusal_page(&[], None, PageRequest::default())
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, Some(0));
    }

    #[test]
    fn guarded_refusal_page_rejects_wrong_batch_length_and_refusal_index() {
        let (edges, mut outcome) = guarded_refusal_fixture(3, 1);
        let error = outcome
            .refusal_page(&edges[..2], None, PageRequest::default())
            .unwrap_err();
        assert!(error.to_string().contains("does not match attempted count"));
        outcome.refused.as_mut().unwrap().entry_index = edges.len();
        let error = outcome
            .refusal_page(&edges, None, PageRequest::default())
            .unwrap_err();
        assert!(error.to_string().contains("refusal index 3 is outside"));
        let empty = GuardedBatchOutcome {
            summary: BatchWriteSummary::default(),
            refused: outcome.refused,
        };
        assert!(empty
            .refusal_page(&[], None, PageRequest::default())
            .is_err());
    }

    #[test]
    fn lazy_refusal_formats_only_sampled_details_with_identical_summary() {
        let total = MAX_BATCH_WRITE_ERROR_DETAILS + 7;
        let calls = std::cell::Cell::new(0);
        let mut eager = BatchWriteSummary::default();
        let mut lazy = BatchWriteSummary::default();
        for index in 0..total {
            let class = BatchWriteErrorClass::InvalidInput;
            let retryability = BatchWriteRetryability::Permanent;
            eager.record_failure(
                index,
                Some(index.to_string()),
                class,
                retryability,
                "bad item",
            );
            lazy.record_failure_with(index, class, retryability, || {
                calls.set(calls.get() + 1);
                (Some(index.to_string()), "bad item".into())
            });
        }
        assert_eq!(calls.get(), MAX_BATCH_WRITE_ERROR_DETAILS);
        assert_eq!(
            serde_json::to_value(lazy).unwrap(),
            serde_json::to_value(eager).unwrap()
        );
    }

    #[test]
    fn lazy_refusal_preserves_first_error_after_empty_sample_messages() {
        let mut summary = BatchWriteSummary::default();
        for index in 0..MAX_BATCH_WRITE_ERROR_DETAILS {
            summary.record_failure(
                index,
                None,
                BatchWriteErrorClass::Unknown,
                BatchWriteRetryability::Unknown,
                "",
            );
        }
        summary.record_failure_with(
            MAX_BATCH_WRITE_ERROR_DETAILS,
            BatchWriteErrorClass::Driver,
            BatchWriteRetryability::Unknown,
            || (None, "first nonempty error".into()),
        );
        assert_eq!(summary.first_error, "first nonempty error");
        assert_eq!(summary.errors.len(), MAX_BATCH_WRITE_ERROR_DETAILS);
        assert_eq!(summary.errors_omitted, 1);
    }

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
