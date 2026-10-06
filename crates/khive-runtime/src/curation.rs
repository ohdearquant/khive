// Licensed under the Apache License, Version 2.0.

//! Curation operations: entity update/merge and edge-list filter type.

use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_db::{pool::RuntimeWriteOperation, SqliteError};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
use khive_storage::types::{EdgeFilter, PageRequest, SqlValue, TextDocument};
use khive_storage::{AtomicUnitOp, EdgeRelation, Entity, SqlStatement, SubstrateKind};
use khive_types::{Details, EdgeEndpointRule, EventKind, KhiveError};
use rusqlite::OptionalExtension;

use crate::error::{RuntimeError, RuntimeResult};
use crate::event_store_guard::EventAttribution;
use crate::operations::{base_entity_rule_allows, canonical_edge_endpoints, endpoint_matches};
use crate::runtime::{KhiveRuntime, NamespaceToken};

mod embedding_text;
mod properties;

pub use embedding_text::{
    entity_embedding_text, entity_fts_document, note_embedding_text, note_fts_document,
};
pub(crate) use embedding_text::{note_embedding_text_ref, note_fts_scalars};
#[cfg(test)]
use properties::merge_json;
pub(crate) use properties::{
    count_new_property_keys, kind_owned_properties, merge_properties, merge_string_field,
    owner_established_property_named_in, preserve_owner_established_properties, union_tags,
    OWNER_ESTABLISHED_PROPERTIES,
};
use properties::{
    message_is_quarantined, note_update_values_equal, preserve_property_keys,
    reject_pack_managed_schedule_mutation,
};

mod note_curation;
mod note_merge;
mod note_merge_guard;
pub(crate) mod note_reindex;
mod outbound_messages;

pub use note_merge_guard::{GuardedNoteMerge, MergeAssertion, NoteMergeGuard};

/// Restrict an outbox scan before its SQL page bound is applied. A held row
/// for another or unconfigured channel must not consume a channel's page.
enum OutboxSlugFilter<'a> {
    Any,
    Exact(&'a str),
    Missing,
}

/// Test-only pause point at the read/write boundary of a guarded
/// read-modify-write, so a race between two concurrent callers of the same
/// PRODUCTION entry point (not the underlying store primitive) can be
/// reproduced deterministically instead of relying on scheduler luck or
/// sleeps. A no-op unless the calling task runs inside
/// `AFTER_READ_BARRIER.scope(...)`; production code never establishes that
/// scope, so `pause_after_read` costs nothing outside these regression
/// tests, and it does not exist at all in non-test builds.
#[cfg(test)]
#[path = "curation/race_seam_tests.rs"]
pub(crate) mod race_seam;

mod types_and_guards;

use types_and_guards::{
    append_merge_event_in_transaction, edge_row_budget_bytes, map_merge_entity_storage_error,
    map_merge_note_storage_error, EmbeddingModelPlan, EntityMergeRefusal, EntityMergeValidation,
    MergeEventContext, MergeSqlError, MergeTxBudget,
};
pub use types_and_guards::{
    entity_merge_guard_compared_values, entity_merge_guard_error,
    entity_merge_guard_refusal_message, validate_entity_merge_floor, ContentMergeStrategy,
    EdgeListFilter, EdgePatch, EntityDedupMergePolicy, EntityMergeGuard, EntityPatch,
    MergeEdgeConflictPreimage, MergeEdgePreimage, MergeSummary, MergeTxBudgetReport, MergeTxLimits,
    NotePatch, NoteUpdatePolicy,
};
pub(crate) use types_and_guards::{
    normalize_note_update_tags, stale_edge_snapshot_error, stale_entity_snapshot_error,
    stale_note_snapshot_error,
};
#[cfg(test)]
use types_and_guards::{MERGE_TX_MAX_BYTES, MERGE_TX_MAX_ROWS};

mod merge_sql;
use merge_sql::{merge_entity_sql, merge_note_sql};

mod entity_curation;
mod merge_edges;

use merge_edges::{
    collect_merge_drop_incident_edge_preimages, delete_merge_drop_edges, edge_row_preimage,
    merge_rewire_endpoint_contract_allows, resolve_merge_edge_endpoint_budgeted, EdgeRow,
};

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// SQL-bind–ready scalars derived from [`note_fts_document`].
///
/// Used by `merge_note_sql` to guarantee that the raw SQL FTS INSERT stores
/// exactly what [`Fts5TextSearch::upsert_document`] would write, preventing
/// null/empty-string divergence on the `title` column for nameless notes.
pub(crate) struct NoteFtsScalars {
    /// Granular note kind used by the indexed corpus classifier.
    pub record_kind: String,
    /// Empty string when `note.name` is `None` — matches the `unwrap_or("")` in
    /// `Fts5TextSearch::upsert_document`.
    pub title: String,
    pub body: String,
    /// Always the JSON array `"[]"`.
    pub tags: String,
    /// Serialised `note.properties`, or `None` when properties are absent.
    pub metadata: Option<String>,
    /// `note.updated_at` converted to `DateTime<Utc>` timestamp_micros.
    pub updated_at_micros: i64,
}

// ---------------------------------------------------------------------------
// INLINE TEST JUSTIFICATION: tests here exercise patch/merge helpers and the
// update_note/update_entity paths that share private merge_properties logic.
// Moving them to tests/ would require pub-exporting merge_properties, which is
// an internal invariant not suitable for the public API surface. Broad
// behavioral curation tests live in tests/integration.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod merge_reservation_tests;

#[cfg(test)]
#[path = "curation_tests.rs"]
mod tests;
