// See docs/operations.md#module-layout.
//! High-level operations composing storage capabilities into user-facing verbs.
//!
//! Fault-injection arming uses scoped guards only — see docs/operations.md#fault-injection-arm-migration.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::Utc;
use serde::Serialize;
use uuid::Uuid;

#[cfg(test)]
use khive_score::DeterministicScore;
use khive_storage::entity::EntityTypeCounts;
#[cfg(test)]
use khive_storage::graph::{SymmetricEdgeUpdateOutcome, SymmetricEdgeUpdateRequest};
use khive_storage::note::Note;
#[cfg(test)]
use khive_storage::types::DeleteMode;
use khive_storage::types::{
    DirectedNeighborHit, Direction, EdgeUpsertDisposition, EdgeUpsertRefusal, EdgeUpsertRequest,
    EdgeUpsertResult, GraphPath, GuardedEdgeUpsertOutcome, LinkId, NeighborCursor, NeighborHit,
    NeighborQuery, PageRequest, SqlStatement, SqlValue, TextFilter, TextQueryMode,
    TextSearchRequest, TraversalRequest,
};
use khive_storage::{
    Attachment, AttachmentSubstrate, Edge, EdgeRelation, Entity, EntityFilter, Event, EventFilter,
    NewAttachment,
};
use khive_types::{EdgeEndpointRule, EndpointKind, EventKind, KhiveError, SubstrateKind};

use khive_db::stores::entity::entity_upsert_statement;

use khive_db::stores::graph::{
    compose_graph_mutation_events, GraphMutationOutcome, GraphMutationPreconditions,
    GraphMutationRequest,
};

use khive_db::stores::text::insert_document_statements;

mod note_search;
mod quarantine_note_retention;
pub use quarantine_note_retention::{QuarantineOriginalRetention, TrustedIngestAttachmentOutcome};

#[cfg(test)]
mod batch_edge_tests;

struct EdgeReadWindow {
    outcomes: Vec<Option<RuntimeResult<Option<Edge>>>>,
    groups: Vec<(khive_types::Namespace, Vec<usize>)>,
}

/// The restore unit committed the row and its text index; only the
/// post-commit embedding rebuild failed. Name that, so the caller does not
/// read an ordinary restore failure over a record that is already live.
fn restore_reindex_failed(kind: &str, id: Uuid, error: RuntimeError) -> RuntimeError {
    RuntimeError::Internal(format!(
        "{kind} {id} is restored and text-indexed, but its embedding rebuild failed \
         and will be retried by the next reindex: {error}"
    ))
}

fn merge_tombstone_restore_refused(id: Uuid, kept_id: impl std::fmt::Display) -> RuntimeError {
    KhiveError::conflict(format!(
        "merge_tombstone: {id} was merged into {kept_id}; a merge tombstone is not restorable, query the kept id"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "merge_tombstone".into()),
        ("merged_into", kept_id.to_string()),
    ]))
    .into()
}

/// The entity total and optional type report consumed together by `stats`.
#[derive(Debug, PartialEq, Eq)]
pub struct EntityStatsCounts {
    pub entities: u64,
    pub entities_by_type: Option<EntityTypeCounts>,
}

/// The live edge total and its two breakdowns consumed together by `stats`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EdgeStatsCounts {
    pub edges: u64,
    pub edges_by_relation: HashMap<String, u64>,
    pub edges_by_endpoint_base: khive_storage::types::EdgeEndpointBaseCounts,
}

/// Count caller-visible live entities through one store. A supported breakdown
/// supplies its own scalar total; only an unavailable report uses legacy counting.
pub async fn entity_stats_counts(
    store: &dyn khive_storage::EntityStore,
    token: &NamespaceToken,
) -> RuntimeResult<EntityStatsCounts> {
    let namespaces: Vec<String> = token
        .visible_namespaces()
        .iter()
        .map(|namespace| namespace.as_str().to_owned())
        .collect();
    match store.count_entities_by_type(&namespaces).await? {
        Some(groups) => {
            let entities = groups.iter().try_fold(0_u64, |total, (_, count)| {
                total.checked_add(*count).ok_or_else(|| {
                    RuntimeError::Internal(
                        "entity type counts exceed the scalar count range".into(),
                    )
                })
            })?;
            Ok(EntityStatsCounts {
                entities,
                entities_by_type: Some(groups),
            })
        }
        None => {
            let entities = store
                .count_entities(
                    token.namespace().as_str(),
                    EntityFilter {
                        namespaces,
                        ..EntityFilter::default()
                    },
                )
                .await?;
            Ok(EntityStatsCounts {
                entities,
                entities_by_type: None,
            })
        }
    }
}

/// Inputs for a store-owned entity identity. Unlike ordinary creation, a
/// competing insert must leave the existing row untouched.
pub struct EntityClaimSpec {
    pub id: Uuid,
    pub kind: String,
    pub entity_type: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub properties: Option<serde_json::Value>,
    pub tags: Vec<String>,
    pub identity_tag: String,
}

fn live_merged_entity_refused(id: Uuid, kept_id: impl std::fmt::Display) -> RuntimeError {
    KhiveError::conflict(format!(
        "live_merged_entity: {id} is live but still carries merged_into {kept_id}; a row an \
         earlier restore left live over its merge is not restorable, re-tombstone it or query the \
         kept id"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "live_merged_entity".into()),
        ("merged_into", kept_id.to_string()),
    ]))
    .into()
}

fn restore_key_conflict(key: &str, holder: &Note) -> RuntimeError {
    KhiveError::conflict(format!(
        "restore_key_conflict: key {key:?} is already held by live note {}",
        holder.id
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "restore_key_conflict".into()),
        ("key", key.to_owned()),
        ("existing_id", holder.id.to_string()),
    ]))
    .into()
}

use crate::atomic_plan::{
    AddEntityPlan, AffectedRowGuard, PlanStatement, PostCommitEffect, UpdatePlan,
};
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use crate::curation::{entity_fts_document, note_embedding_text_ref, note_fts_document};
use crate::error::{GuardedWriteFailure, RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

/// A non-retryable failure after a substrate mutation has committed.
/// Callers can report the committed id/result and this diagnostic together.
#[derive(Clone, Debug, Serialize)]
pub struct PostCommitDegradation {
    pub stage: &'static str,
    pub error: String,
}

impl PostCommitDegradation {
    fn new(stage: &'static str, error: impl ToString) -> Self {
        Self {
            stage,
            error: error.to_string(),
        }
    }
}

// One list declares the stages, so the variants, `ALL` and the labels cannot drift apart.
macro_rules! conditional_insert_stages {
    ($($stage:ident => $label:literal),+ $(,)?) => {
        /// The post-commit stages a conditional note insert reports in its committed
        /// `post_commit_degraded` outcome. The comm ingest decoder accepts a stage only
        /// when it is the label of a member of [`ConditionalInsertStage::ALL`], and the
        /// insert can record a stage only through this type, so every label the
        /// runtime emits is one the decoder accepts.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum ConditionalInsertStage {
            $($stage,)+
        }

        impl ConditionalInsertStage {
            /// Every stage, generated from the same list as the variants.
            pub const ALL: &'static [Self] = &[$(Self::$stage,)+];

            pub const fn label(self) -> &'static str {
                match self {
                    $(Self::$stage => $label,)+
                }
            }
        }
    };
}

conditional_insert_stages! {
    FtsAcquisition => "fts_acquisition",
    FtsUpsert => "fts_upsert",
    Embedding => "embedding",
    VectorAcquisition => "vector_acquisition",
    VectorInsert => "vector_insert",
}

impl ConditionalInsertStage {
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|stage| stage.label() == label)
    }
}

fn record_conditional_insert_degradation(
    degradations: &mut Vec<PostCommitDegradation>,
    id: Uuid,
    stage: ConditionalInsertStage,
    error: impl ToString,
) {
    record_post_commit_degradation(degradations, "try_create_note", id, stage.label(), error);
}

fn record_post_commit_degradation(
    degradations: &mut Vec<PostCommitDegradation>,
    operation: &'static str,
    id: Uuid,
    stage: &'static str,
    error: impl ToString,
) {
    let degradation = PostCommitDegradation::new(stage, error);
    tracing::warn!(%operation, %id, stage, error = %degradation.error,
        "substrate mutation committed with post-commit degradation");
    degradations.push(degradation);
}

/// Legacy methods cannot return both their original result and the new report.
/// Give direct callers a typed, non-retryable error with the committed record
/// handle and every failed stage instead of silently dropping the report.
pub(crate) fn legacy_post_commit_result<T>(
    operation: &'static str,
    id: Uuid,
    value: T,
    degradations: Vec<PostCommitDegradation>,
) -> RuntimeResult<T> {
    if degradations.is_empty() {
        return Ok(value);
    }
    let failures = serde_json::Value::Array(
        degradations
            .iter()
            .map(|failure| {
                serde_json::json!({"stage": failure.stage, "error": failure.error.as_str()})
            })
            .collect(),
    );
    Err(KhiveError::internal(format!(
        "{operation} committed record {id}, but post-commit work failed; do not retry the mutation; reconcile by record_id"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "post_commit_degraded".to_string()),
        ("operation", operation.to_string()),
        ("record_id", id.to_string()),
        ("committed", "true".to_string()),
        ("retryable", "false".to_string()),
        ("post_commit_degradations", failures.to_string()),
    ]))
    .into())
}

pub(crate) fn legacy_post_commit_result_with_embedding<T>(
    operation: &'static str,
    id: Uuid,
    value: T,
    embedding: crate::retrieval::EmbeddingTruncationReport,
    degradations: Vec<PostCommitDegradation>,
) -> RuntimeResult<T> {
    if !embedding.any_truncated() {
        return legacy_post_commit_result(operation, id, value, degradations);
    }
    let failures = serde_json::Value::Array(
        degradations
            .iter()
            .map(|failure| {
                serde_json::json!({"stage": failure.stage, "error": failure.error.as_str()})
            })
            .collect(),
    );
    Err(KhiveError::internal(format!(
        "{operation} committed record {id}, but embedding input was truncated; do not retry the mutation; reconcile by record_id"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "embedding_input_truncated".to_string()),
        ("operation", operation.to_string()),
        ("record_id", id.to_string()),
        ("committed", "true".to_string()),
        ("retryable", "false".to_string()),
        (
            "embedding_truncation_report",
            serde_json::json!(embedding).to_string(),
        ),
        ("post_commit_degradations", failures.to_string()),
    ]))
    .into())
}

mod attachments;
mod entity_bulk;
mod entity_create;
mod entity_indexes;
#[cfg(any(test, feature = "fault-injection"))]
mod fault_injection;
mod resolve_prefix_in;
mod resolve_uuid_or_prefix;

mod delete_restore;
mod edge_query;
mod graph_rules;
mod identity_reads;
mod link_ops;
mod read_helpers;
mod record_ops;
mod traversal;

use read_helpers::{
    direction_sort_rank, drain_embed_join_set, normalize_symmetric_direction, note_snippet,
    note_title, resolve_prefix_statement, salience_weighted_rank,
};
pub use read_helpers::{
    hex_prefix_to_uuid_pattern, uuid_prefix_bounds, NoteSearchHit, NoteSearchOutcome, Resolved,
};
pub use record_ops::NoteCreateSpec;

pub use edge_query::QueryResult;
use graph_rules::{accepted_entity_relations_description, static_impossible_edge_pattern_warnings};
pub use graph_rules::{
    accepted_entity_relations_for_entities, accepted_pack_relations_for_entities,
    dependency_kind_inference_hint, merge_entry_metadata,
};
pub(crate) use graph_rules::{
    canonical_edge_endpoint_kinds, canonical_edge_endpoints, merge_dependency_kind,
    pack_rule_allows, validate_edge_metadata, validate_edge_weight,
};
#[cfg(test)]
use link_ops::guarded_link_batch_failure;
#[cfg(test)]
use traversal::merge_traversal_paths_by_root;

#[cfg(any(test, feature = "fault-injection"))]
pub use fault_injection::{
    arm_entity_compensation_fail_scoped, arm_fts_fail_many_partial_scoped,
    arm_fts_fail_many_scoped, arm_fts_fail_scoped, arm_fts_search_fail,
    arm_prefix_resolve_fail_scoped, arm_rollback_cleanup_fail, arm_vector_fail_after,
    arm_vector_fail_scoped, FaultInjectionArm,
};
#[cfg(test)]
use fault_injection::{arm_fault, FaultArmSet, LINK_FAIL_AFTER};
#[cfg(any(test, feature = "fault-injection"))]
use fault_injection::{
    consume_fault, ENTITY_COMPENSATION_FAIL_NS, FTS_FAIL_MANY_NS, FTS_FAIL_MANY_PARTIAL_NS,
    FTS_FAIL_NS, FTS_SEARCH_FAIL_NS, PREFIX_RESOLVE_FAIL_NS, ROLLBACK_CLEANUP_FAIL_NS,
    VECTOR_FAIL_AFTER, VECTOR_FAIL_NS,
};
#[cfg(any(test, feature = "fault-injection"))]
pub(crate) use fault_injection::{consume_fts_fail_fault, consume_vector_fail_fault};
pub use resolve_prefix_in::PrefixScope;

/// A by-ID edge-endpoint substrate kind, including `Edge` itself.
///
/// Unlike [`Resolved`], this carries no record data — it is used where only
/// the substrate classification is needed (coordinator locate/link parity
/// with `get`, ADR-002 rule 1: `annotates` target may be entity, note, edge,
/// or event).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeEndpointKind {
    Entity,
    Note,
    Event,
    Edge,
}

impl EdgeEndpointKind {
    /// Wire name carried by a link lifecycle event for this endpoint.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Note => "note",
            Self::Event => "event",
            Self::Edge => "edge",
        }
    }
}

/// Map a resolved endpoint to its `(substrate, kind, entity_type)` triple, or
/// `None` if the substrate is not a valid edge endpoint (events, edges).
///
/// `entity_type` carries the pack-owned granular subtype (`Entity::entity_type`,
/// e.g. `"theorem"`); it is `None` for notes and for entities with no subtype.
fn resolved_pair(r: Option<&Resolved>) -> Option<(&'static str, &str, Option<&str>)> {
    match r? {
        Resolved::Entity(e) => Some(("entity", e.kind.as_str(), e.entity_type.as_deref())),
        Resolved::Note(n) => Some(("note", n.kind.as_str(), None)),
        Resolved::Event(_) => None,
        Resolved::PackRecord { .. } => None,
    }
}

pub use khive_types::{
    base_entity_endpoint_rules, base_entity_rule_allows, endpoint_matches,
    BASE_ENTITY_ENDPOINT_RULES,
};

impl KhiveRuntime {
    // ---- Note operations ----

    /// Create and persist a note, optionally with properties and annotation targets.
    ///
    /// After creating the note:
    /// - Always indexes into FTS5 at the `notes_<namespace>` key.
    /// - If an embedding model is configured, indexes into the vector store with
    ///   `SubstrateKind::Note`.
    /// - For each UUID in `annotates`, creates an `EdgeRelation::Annotates` edge from
    ///   the note to that target.
    ///
    /// A bounded embedding returns a non-retryable error carrying the committed
    /// note ID and truncation report. The report-aware embedding-content variant
    /// accepts `None` to retain the same default input selection.
    // REASON: note creation requires kind, name, content, salience, properties, annotates,
    // and namespace token — mirrors the MCP verb surface; a builder would not reduce
    // caller complexity for pack handler callers.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        let (note, embedding, degradations, _) = self
            .create_note_inner(
                token, kind, name, content, None, salience, None, properties, annotates, None,
                false, false,
            )
            .await?;
        legacy_post_commit_result_with_embedding(
            "create_note",
            note.id,
            note,
            embedding,
            degradations,
        )
    }

    /// Publish a network receipt with provenance that generic note writes
    /// cannot supply. The web pack provides only the request record and the
    /// annotation targets; this entry point fixes the note kind, tag, and
    /// provenance before the first storage write.
    pub async fn create_web_receipt_note(
        &self,
        token: &NamespaceToken,
        summary: &str,
        request: serde_json::Value,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        let properties = serde_json::json!({
            "tags": ["web.receipt"],
            "request": request,
        });
        let (note, _, degradations, _) = self
            .create_note_inner(
                token,
                "observation",
                None,
                summary,
                None,
                None,
                None,
                Some(properties),
                annotates,
                None,
                false,
                true,
            )
            .await?;
        legacy_post_commit_result("create_web_receipt_note", note.id, note, degradations)
    }

    /// Like [`Self::create_note`], but lets the caller supply a smaller text
    /// to send to the vector embedder while the note's stored/FTS-indexed
    /// `content` remains the full text.
    ///
    /// `embedding_content`, when `Some`, must be non-empty and a proper
    /// prefix of `content` — anything else is rejected with `InvalidInput`
    /// before any write. `None` behaves exactly like [`Self::create_note`].
    /// Use this when `content` may exceed an embedder's input cap (e.g. a
    /// very long commit message) and only a capped head prefix should be
    /// embedded, while the full text is still stored and searchable via FTS.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_embedding_content(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        embedding_content: Option<&str>,
        salience: Option<f64>,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        let (note, embedding, degradations, _) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                embedding_content,
                salience,
                None,
                properties,
                annotates,
                None,
                false,
                false,
            )
            .await?;
        legacy_post_commit_result_with_embedding(
            "create_note_with_embedding_content",
            note.id,
            note,
            embedding,
            degradations,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_embedding_content_and_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        embedding_content: Option<&str>,
        salience: Option<f64>,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<(Note, crate::retrieval::EmbeddingTruncationReport)> {
        let (note, embedding, degradations, _) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                embedding_content,
                salience,
                None,
                properties,
                annotates,
                None,
                false,
                false,
            )
            .await?;
        legacy_post_commit_result(
            "create_note_with_embedding_content_and_report",
            note.id,
            (note, embedding),
            degradations,
        )
    }

    /// The committed note and its non-retryable post-commit diagnostics.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_embedding_content_and_post_commit_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        embedding_content: Option<&str>,
        salience: Option<f64>,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<(
        Note,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        let (note, embedding, degradations, _) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                embedding_content,
                salience,
                None,
                properties,
                annotates,
                None,
                false,
                false,
            )
            .await?;
        Ok((note, embedding, degradations))
    }

    /// Like [`Self::create_note`] but also sets a non-zero decay factor on the note.
    // REASON: extends create_note with an additional decay_factor parameter; same
    // rationale — mirrors the MCP surface and reduces an extra builder layer.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<Note> {
        self.create_note_with_decay_for_embedding_model(
            token,
            kind,
            name,
            content,
            salience,
            decay_factor,
            properties,
            annotates,
            None,
        )
        .await
    }

    /// Create a note with decay and retain embedding truncation accounting.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay_and_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
    ) -> RuntimeResult<(Note, crate::retrieval::EmbeddingTruncationReport)> {
        self.create_note_with_decay_for_embedding_model_and_report(
            token,
            kind,
            name,
            content,
            salience,
            decay_factor,
            properties,
            annotates,
            None,
        )
        .await
    }

    /// Like [`Self::create_note_with_decay`] but targets a specific embedding model.
    // REASON: adds an embedding_model parameter to the decay variant; the full parameter
    // set is required for correct MCP verb routing and cannot be collapsed without
    // introducing a separate config struct that would obscure call sites.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay_for_embedding_model(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
        embedding_model: Option<&str>,
    ) -> RuntimeResult<Note> {
        let (note, embedding, degradations, _) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                None,
                salience,
                Some(decay_factor),
                properties,
                annotates,
                embedding_model,
                false,
                false,
            )
            .await?;
        legacy_post_commit_result_with_embedding(
            "create_note_with_decay_for_embedding_model",
            note.id,
            note,
            embedding,
            degradations,
        )
    }

    /// Create a note with decay for a named model and retain embedding truncation accounting.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay_for_embedding_model_and_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
        embedding_model: Option<&str>,
    ) -> RuntimeResult<(Note, crate::retrieval::EmbeddingTruncationReport)> {
        let (note, embedding, degradations, _) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                None,
                salience,
                Some(decay_factor),
                properties,
                annotates,
                embedding_model,
                false,
                false,
            )
            .await?;
        legacy_post_commit_result(
            "create_note_with_decay_for_embedding_model_and_report",
            note.id,
            (note, embedding),
            degradations,
        )
    }

    /// Memory-pack receipt form of the decay create. Each returned sequence
    /// was read inside the transaction that inserted that model's vector and
    /// ANN upsert log row; a failed or version-rejected vector writes no fence.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay_for_embedding_model_with_visibility(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
        embedding_model: Option<&str>,
    ) -> RuntimeResult<(Note, Vec<(String, u64)>)> {
        let (note, _, degradations, fences) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                None,
                salience,
                Some(decay_factor),
                properties,
                annotates,
                embedding_model,
                true,
                false,
            )
            .await?;
        legacy_post_commit_result(
            "create_note_with_decay_for_embedding_model_with_visibility",
            note.id,
            (note, fences),
            degradations,
        )
    }

    /// Memory-pack create returning both its committed vector fences and
    /// embedding-input truncation accounting. Other post-commit degradations
    /// retain the same non-retryable error behavior as the receipt-only API.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_decay_for_embedding_model_with_visibility_and_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        salience: Option<f64>,
        decay_factor: f64,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
        embedding_model: Option<&str>,
    ) -> RuntimeResult<(
        Note,
        Vec<(String, u64)>,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let (note, embedding, degradations, fences) = self
            .create_note_inner(
                token,
                kind,
                name,
                content,
                None,
                salience,
                Some(decay_factor),
                properties,
                annotates,
                embedding_model,
                true,
                false,
            )
            .await?;
        legacy_post_commit_result(
            "create_note_with_decay_for_embedding_model_with_visibility_and_report",
            note.id,
            (note, fences, embedding),
            degradations,
        )
    }

    /// Insert a note using `INSERT OR IGNORE` semantics for atomic deduplication.
    ///
    /// Returns `Ok(Some(note))` when the note was newly written.  Returns
    /// `Ok(None)` when a unique constraint (e.g. the channel-scoped `external_id`
    /// partial index on comm message notes) was already satisfied by an existing row,
    /// making this call a no-op. Indexing failures after an insert return a
    /// typed `post_commit_degraded` error with the committed record ID and
    /// `retryable=false`. Healthy models still run; the note is not rolled back.
    ///
    /// This method is intentionally narrower than `create_note`: it skips
    /// salience/decay, annotates edges, and embedding-model selection, which
    /// are not needed for channel-ingest paths.
    ///
    /// Rejects the transport-owned properties on a `message` note: the
    /// `message` entry of the kind-owned property list that the note-store
    /// accessor guard enforces, so both guards refuse the same keys. These
    /// properties are transport-owned evidence (`comm.health` trusts the
    /// quarantine and channel ones at face value), and this fast path (unlike
    /// the generic `create` verb funnel) is not covered by the
    /// pack-installed note-write validator. Only the trusted channel-ingest
    /// path may establish them — see
    /// [`Self::try_create_note_as_trusted_ingest`].
    pub async fn try_create_note(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
    ) -> RuntimeResult<Option<Note>> {
        self.try_create_note_impl(
            token, kind, name, content, properties, false, None, None, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_create_note_impl(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        allow_transport_owned_message_properties: bool,
        attachment: Option<NewAttachment>,
        expires_after: Option<std::time::Duration>,
        retention: Option<&mut QuarantineOriginalRetention>,
    ) -> RuntimeResult<Option<Note>> {
        self.validate_note_kind(kind)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        crate::secret_gate::check_note_fields_at(content, name, properties.as_ref(), "note")?;
        let secondary_quarantine = allow_transport_owned_message_properties
            && kind == "message"
            && self.backend_id().as_str() != crate::BackendId::MAIN
            && attachment
                .as_ref()
                .is_some_and(|attachment| attachment.role == "quarantine-original")
            && properties.as_ref().is_some_and(|properties| {
                properties.get("quarantined") == Some(&serde_json::Value::Bool(true))
                    || properties
                        .get("quarantined")
                        .and_then(serde_json::Value::as_str)
                        == Some("true")
            });
        if let Some(ref attachment) = attachment {
            if !secondary_quarantine {
                // Ordinary attachment writes still require the main backend.
                drop(self.attachments()?);
            }
            attachment.validate()?;
            if !secondary_quarantine {
                let blob_store = self.blob_store().ok_or_else(|| {
                    RuntimeError::Unconfigured(
                        "trusted ingest attachment requires an installed BlobStore".to_string(),
                    )
                })?;
                if !blob_store.exists(&attachment.content_ref).await? {
                    return Err(RuntimeError::InvalidInput(format!(
                        "trusted ingest attachment refers to an unpublished blob: {}",
                        attachment.content_ref
                    )));
                }
            }
        }
        if !allow_transport_owned_message_properties && kind == "message" {
            if let Some(key) = properties
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .and_then(|supplied| {
                    crate::curation::kind_owned_properties("message")
                        .iter()
                        .copied()
                        .find(|key| supplied.contains_key(*key))
                })
            {
                return Err(RuntimeError::InvalidInput(format!(
                    "`{key}` is transport-owned on a `message` note and cannot be supplied \
                     through `try_create_note`; only the trusted channel-ingest path may \
                     establish quarantine disposition and channel provenance"
                )));
            }
        }

        let ns = token.namespace().as_str();
        let mut note = Note::new(ns, kind, content);
        if let Some(retention) = expires_after {
            let duration_us = i64::try_from(retention.as_micros()).map_err(|_| {
                RuntimeError::InvalidInput(
                    "trusted ingest retention exceeds i64 microseconds".into(),
                )
            })?;
            note.expires_at = Some(note.created_at.checked_add(duration_us).ok_or_else(|| {
                RuntimeError::InvalidInput("trusted ingest expiry exceeds i64 microseconds".into())
            })?);
        }
        if let Some(n) = name {
            note = note.with_name(n);
        }
        if let Some(p) = properties {
            note = note.with_properties(p);
        }

        // Bypasses the `notes()` accessor's PolicyEnforcingNoteStore wrapper —
        // the reserved-transport-property check above already enforces the
        // identical policy, conditionally allowing the trusted-ingest path,
        // so this reaches storage directly rather than duplicate the check
        // through a wrapper that cannot see the trust decision this function
        // just made.
        let inserted = if let Some(attachment) = attachment {
            if secondary_quarantine {
                let mut outcome = QuarantineOriginalRetention::Retained;
                let result = self
                    .try_insert_secondary_quarantine_note(
                        token,
                        &mut note,
                        attachment,
                        &mut outcome,
                    )
                    .await;
                if let Some(retention) = retention {
                    *retention = outcome;
                }
                result?
            } else {
                self.raw_notes(token)?
                    .try_insert_note_with_attachments(
                        note.clone(),
                        vec![Attachment::from_new(
                            note.id,
                            AttachmentSubstrate::Note,
                            attachment,
                            note.created_at,
                        )],
                    )
                    .await?
            }
        } else {
            self.raw_notes(token)?.try_insert_note(note.clone()).await?
        };
        if !inserted {
            return Ok(None);
        }

        let mut degradations = Vec::new();
        match self.text_for_notes(token) {
            Ok(fts) => {
                if let Err(error) = fts.upsert_document(note_fts_document(&note)).await {
                    record_conditional_insert_degradation(
                        &mut degradations,
                        note.id,
                        ConditionalInsertStage::FtsUpsert,
                        error,
                    );
                }
            }
            Err(error) => record_conditional_insert_degradation(
                &mut degradations,
                note.id,
                ConditionalInsertStage::FtsAcquisition,
                error,
            ),
        }

        let embed_model_names = self.embedding_models_for_note_kind(kind);
        for model_name in &embed_model_names {
            match self
                .embed_document_with_model_outcome_for_token(
                    token,
                    model_name,
                    note_embedding_text_ref(&note),
                )
                .await
            {
                Ok(outcome) => {
                    if outcome.truncated {
                        tracing::warn!(
                            note_id = %note.id,
                            model = %outcome.model_name,
                            source_bytes = outcome.source_bytes,
                            embedded_bytes = outcome.embedded_bytes,
                            "try_create_note: embedding input truncated; full content stored unchanged"
                        );
                    }
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => {
                            if let Err(error) = vs
                                .insert(
                                    note.id,
                                    SubstrateKind::Note,
                                    ns,
                                    "note.content",
                                    vec![outcome.vector],
                                )
                                .await
                            {
                                record_conditional_insert_degradation(
                                    &mut degradations,
                                    note.id,
                                    ConditionalInsertStage::VectorInsert,
                                    format!("model {model_name}: {error}"),
                                );
                            }
                        }
                        Err(error) => record_conditional_insert_degradation(
                            &mut degradations,
                            note.id,
                            ConditionalInsertStage::VectorAcquisition,
                            format!("model {model_name}: {error}"),
                        ),
                    }
                }
                Err(error) => record_conditional_insert_degradation(
                    &mut degradations,
                    note.id,
                    ConditionalInsertStage::Embedding,
                    format!("model {model_name}: {error}"),
                ),
            }
        }

        legacy_post_commit_result("try_create_note", note.id, Some(note), degradations)
    }

    // REASON: private inner function unifies all create_note variants; it receives every
    // optional parameter individually so that public variants can pass None without
    // requiring callers to construct an intermediate struct.
    #[allow(clippy::too_many_arguments)]
    async fn create_note_inner(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        embedding_content: Option<&str>,
        salience: Option<f64>,
        decay_factor: Option<f64>,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
        embedding_model: Option<&str>,
        capture_visibility: bool,
        web_receipt: bool,
    ) -> RuntimeResult<(
        Note,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
        Vec<(String, u64)>,
    )> {
        self.validate_note_kind(kind)?;
        // Owned identity properties are derived from the authorization token
        // before anything else touches them, so every caller of this function —
        // the generic `create` verb and direct Rust callers alike — stores the
        // same derived values. Runs before the secret gate so the gate scans
        // exactly what will be written.
        let mut properties = self.derive_note_write_properties(kind, token, properties)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        if web_receipt {
            let map = properties
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
                .expect("web receipt properties are constructed as an object");
            map.insert(
                crate::secret_gate::RESERVED_WEB_RECEIPT_KEY.to_string(),
                serde_json::Value::String(
                    crate::secret_gate::WEB_RECEIPT_PROVENANCE_VALUE.to_string(),
                ),
            );
        }
        // Secret gate: scan content, optional name, and structured properties.
        crate::secret_gate::check_note_fields_at(content, name, properties.as_ref(), "note")?;
        // `embedding_content` is a caller-supplied alternate vector-embedding
        // input: it must be a non-empty proper prefix of `content` (never a
        // superset, an unrelated string, or the full text) and passes the
        // same secret gate as any other stored/embedded text. Rejected
        // before any write, same as the checks above.
        if let Some(ec) = embedding_content {
            if ec.is_empty() {
                return Err(RuntimeError::InvalidInput(
                    "embedding_content must not be empty".into(),
                ));
            }
            if ec.len() >= content.len() || !content.starts_with(ec) {
                return Err(RuntimeError::InvalidInput(
                    "embedding_content must be a proper prefix of content".into(),
                ));
            }
            crate::secret_gate::check_at(ec, "note", "embedding_content")?;
        }
        let ns = token.namespace().as_str();

        // Validate all annotates targets before any write (atomicity: all-or-nothing).
        // Endpoint resolution is by-ID and namespace-agnostic.
        for &target_id in &annotates {
            if !self.substrate_exists_by_id(token, target_id).await? {
                return Err(RuntimeError::NotFound(format!(
                    "create_note annotates target {target_id} not found"
                )));
            }
        }

        // Reject non-finite or out-of-range salience/decay at the runtime boundary
        // rather than letting storage silently clamp them (coding-standards §508-516).
        if let Some(s) = salience {
            if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                return Err(RuntimeError::InvalidInput(format!(
                    "salience must be a finite value in [0.0, 1.0]; got {s}"
                )));
            }
        }
        if let Some(d) = decay_factor {
            if !d.is_finite() || d < 0.0 {
                return Err(RuntimeError::InvalidInput(format!(
                    "decay_factor must be a finite value >= 0.0; got {d}"
                )));
            }
        }

        // Resolve embedding_model BEFORE any note/FTS/vector write so unknown-model
        // errors are atomic at the runtime layer, not just at one pack handler.
        // Direct Rust callers (other packs, integration tests) get the same guarantee.
        if let Some(model_name) = embedding_model {
            self.resolve_embedding_model(Some(model_name))?;
        }

        let mut note = Note::new(ns, kind, content);
        if let Some(s) = salience {
            note = note.with_salience(s);
        }
        if let Some(df) = decay_factor {
            note = note.with_decay(df);
        }
        if let Some(n) = name {
            note = note.with_name(n);
        }
        if let Some(p) = properties {
            note = note.with_properties(p);
        }
        let notes = if web_receipt {
            self.raw_notes(token)?
        } else {
            self.notes(token)?
        };
        notes.upsert_note(note.clone()).await?;

        // From here on, any error must compensate by removing the note row, its
        // FTS document, and any vector entries already inserted — the same
        // cleanup used by the annotates-edge block below.

        // Decide which embedding models to use (before touching FTS/vectors).
        let embed_model_names: Vec<String> = if let Some(m) = embedding_model {
            vec![m.to_string()]
        } else {
            self.embedding_models_for_note_kind(kind)
        };

        // FTS step — compensate note row on failure.
        {
            // Injection: check FTS_FAIL_NS (armed by `arm_fts_fail_scoped(ns)`).
            // Fires only when `ns` is in the armed set, removing it on the way
            // out (one-shot, atomic check-and-remove under the mutex). No lock
            // acquisition in release builds — the cfg(not) branch is a const
            // false so the compiler eliminates the if-branch entirely.
            #[cfg(any(test, feature = "fault-injection"))]
            let fts_inject = consume_fault(&FTS_FAIL_NS, ns);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let fts_inject = false;
            let fts_result: RuntimeResult<()> = if fts_inject {
                Err(RuntimeError::Internal("injected FTS failure".to_string()))
            } else {
                let statements =
                    khive_db::stores::text::delete_document_statements("fts_notes", ns, note.id)
                        .into_iter()
                        .chain(khive_db::stores::text::insert_document_statements(
                            "fts_notes",
                            &note_fts_document(&note),
                        ))
                        .collect();
                self.apply_note_index_revision(&note, statements)
                    .await
                    .map(|_| ())
            };

            if let Err(e) = fts_result {
                self.compensate_note_creation(&note).await;
                return Err(e);
            }
        }

        // Vector embedding + insert step — compensate note row + FTS doc on failure.
        // Multi-model vector embedding:
        //   - explicit embedding_model → single model (existing behaviour)
        //   - None → the note kind's declared model policy
        //   - None + no models configured → skip (text-only)
        // The effective text sent to every embedder: the caller-supplied
        // capped override when present, otherwise the full stored content.
        // FTS indexing above always used the full `note.content` — this cap
        // affects only the vector-embedding input.
        let canonical_embed_text = note_embedding_text_ref(&note);
        let embed_text = embedding_content.unwrap_or(canonical_embed_text);

        let mut embedding_report = crate::retrieval::EmbeddingTruncationReport::default();
        let mut vector_fences = Vec::with_capacity(embed_model_names.len());
        if embed_model_names.len() == 1 {
            // Single-model path: preserves original sequential behaviour.
            let model_name = &embed_model_names[0];
            let vec_result = self
                .embed_document_with_model_outcome_for_token(token, model_name, embed_text)
                .await;

            // Injection: check VECTOR_FAIL_NS (armed by `arm_vector_fail_scoped(ns)`) or
            // VECTOR_FAIL_AFTER (armed by `arm_vector_fail_after(n)`). The former
            // fires only when the armed namespace matches this note's namespace;
            // callers that cannot guarantee no concurrently-running test also
            // writes a note into that same namespace (e.g. a test suite whose
            // fixtures share one default namespace) should prefer the latter,
            // thread-local count instead — see its doc comment. Either clears
            // (one-shot) once it fires. No lock/cell access in release builds —
            // the cfg(not) branch is a const false eliminating the if-branch.
            #[cfg(any(test, feature = "fault-injection"))]
            let vec_inject = {
                let ns_inject = consume_fault(&VECTOR_FAIL_NS, ns);
                let count_inject = VECTOR_FAIL_AFTER.with(|cell| match cell.get() {
                    Some(0) => {
                        cell.set(None);
                        true
                    }
                    Some(n) => {
                        cell.set(Some(n - 1));
                        false
                    }
                    None => false,
                });
                ns_inject || count_inject
            };
            #[cfg(not(any(test, feature = "fault-injection")))]
            let vec_inject = false;
            let vec_result: RuntimeResult<crate::retrieval::DocumentEmbeddingOutcome> =
                if vec_inject {
                    Err(RuntimeError::Internal(
                        "injected vector failure".to_string(),
                    ))
                } else {
                    vec_result
                };

            let single_model_result: RuntimeResult<()> = match vec_result {
                Ok(outcome) => {
                    embedding_report.observe(&outcome);
                    if capture_visibility {
                        match self
                            .publish_note_vector_revision_with_seq(
                                token,
                                &note,
                                model_name,
                                &outcome.vector,
                            )
                            .await
                        {
                            Ok(Some(seq)) => {
                                vector_fences.push((model_name.clone(), seq));
                                Ok(())
                            }
                            Ok(None) => Ok(()),
                            Err(error) => Err(error),
                        }
                    } else {
                        self.publish_note_vector_revision(token, &note, model_name, &outcome.vector)
                            .await
                            .map(|_| ())
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(e) = single_model_result {
                self.compensate_note_creation(&note).await;
                return Err(e);
            }
        } else if !embed_model_names.is_empty() {
            // Multi-model path: embed with each model in parallel via spawned tasks,
            // then insert one VectorRecord per model.
            let rt_clone = self.clone();
            // JoinSet tasks require owned text; an Arc keeps this to one
            // content-sized allocation rather than one clone per model.
            let content_owned: std::sync::Arc<str> = std::sync::Arc::from(embed_text);
            let usage_ctx = crate::usage::current();
            let mut join_set = tokio::task::JoinSet::new();
            for (idx, model_name) in embed_model_names.iter().enumerate() {
                let rt = rt_clone.clone();
                let text = std::sync::Arc::clone(&content_owned);
                let name = model_name.clone();
                let ctx = usage_ctx.clone();
                let token = (*token).clone();
                join_set.spawn(crate::runtime::inherit_request_embedder_scope(async move {
                    let fut = rt.embed_document_with_model_outcome_for_token(
                        &token,
                        &name,
                        text.as_ref(),
                    );
                    let result = match ctx {
                        Some(ctx) => crate::usage::scope(ctx, fut).await,
                        None => fut.await,
                    };
                    (idx, result)
                }));
            }
            // The first failed or panicked handle aborts and detaches its
            // siblings. Embed usage is counted at dispatch, so a synchronous
            // provider winding down in the background cannot change it.
            let outcomes = match drain_embed_join_set(join_set, embed_model_names.len()).await {
                Ok(outcomes) => outcomes,
                Err(e) => {
                    self.compensate_note_creation(&note).await;
                    return Err(e);
                }
            };
            // TODO(P2): parallelize vector inserts
            for (model_name, outcome) in embed_model_names.iter().zip(outcomes) {
                embedding_report.observe(&outcome);
                let insert_result = if capture_visibility {
                    self.publish_note_vector_revision_with_seq(
                        token,
                        &note,
                        model_name,
                        &outcome.vector,
                    )
                    .await
                    .map(|seq| {
                        if let Some(seq) = seq {
                            vector_fences.push((model_name.clone(), seq));
                        }
                    })
                } else {
                    self.publish_note_vector_revision(token, &note, model_name, &outcome.vector)
                        .await
                        .map(|_| ())
                };
                if let Err(e) = insert_result {
                    self.compensate_note_creation(&note).await;
                    return Err(e);
                }
            }
        }

        // Create annotates edges, compensating on failure to preserve atomicity.
        // Pre-validation (above) ensures all targets exist, so link failures are
        // unexpected. If one occurs, purge incident edges and the note in one
        // transaction; a failed purge retains the live note as their source.
        let mut created_edges: Vec<Uuid> = Vec::with_capacity(annotates.len());

        // In test builds, iterate with an index so the failure-injection hook can
        // target a specific call.  In release builds, skip the enumerate overhead.
        #[cfg(test)]
        let annotates_iter: Vec<(usize, Uuid)> = annotates
            .iter()
            .enumerate()
            .map(|(i, &id)| (i, id))
            .collect();
        #[cfg(test)]
        macro_rules! next_target {
            ($pair:expr) => {
                $pair.1
            };
        }
        #[cfg(not(test))]
        let annotates_iter: Vec<Uuid> = annotates.to_vec();
        #[cfg(not(test))]
        macro_rules! next_target {
            ($pair:expr) => {
                $pair
            };
        }

        for pair in annotates_iter {
            let target_id = next_target!(pair);

            // Test-only: inject a failure on the configured call index (1-based).
            #[cfg(test)]
            let injected_err: Option<RuntimeError> = {
                let call_idx = pair.0;
                LINK_FAIL_AFTER.with(|cell| {
                    let n = cell.get();
                    if n > 0 && call_idx + 1 == n {
                        cell.set(0); // reset so subsequent calls are unaffected
                        Some(RuntimeError::Internal("injected link failure".to_string()))
                    } else {
                        None
                    }
                })
            };
            #[cfg(not(test))]
            let injected_err: Option<RuntimeError> = None;

            let link_result = if let Some(e) = injected_err {
                Err(e)
            } else {
                self.link(
                    token,
                    note.id,
                    target_id,
                    EdgeRelation::Annotates,
                    1.0,
                    None,
                )
                .await
            };

            match link_result {
                Ok(edge) => created_edges.push(edge.id.into()),
                Err(e) => {
                    // The graph purge, index cleanup, and note removal share
                    // one writer transaction. If the edge purge fails, its
                    // transaction rolls back, preserving a live source note
                    // for the surviving edges instead of orphaning them.
                    let edge_ids = created_edges
                        .iter()
                        .map(Uuid::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    match self.compensate_note_creation_with_edges(&note).await {
                        Ok(true) => return Err(e),
                        Ok(false) => {
                            return Err(RuntimeError::Internal(format!(
                                "create_note: annotates link failed: {e}; note {} changed before \
                                 compensation, retaining its incident edges [{edge_ids}]",
                                note.id
                            )));
                        }
                        Err(cleanup_error) => {
                            return Err(RuntimeError::Internal(format!(
                                "create_note: annotates link failed: {e}; compensation failed \
                                 for note {} and retained edges [{edge_ids}]: {cleanup_error}; \
                                 note and edges remain for reconciliation",
                                note.id
                            )));
                        }
                    }
                }
            }
        }

        // Same contract as the entity arrival event above: after compensation,
        // so a rolled-back create leaves no event. This is the single funnel for
        // every note create in the product, which is why the memory pack's own
        // note_created emitter was removed rather than left beside it.
        let created_event = khive_storage::event::Event::new(
            note.namespace.clone(),
            "create",
            EventKind::NoteCreated,
            SubstrateKind::Note,
            "",
        )
        .with_target(note.id)
        .with_payload(serde_json::json!({
            "id": note.id,
            "namespace": note.namespace,
            "kind": note.kind,
            "salience": note.salience,
        }));
        let event_result = match self.events(token) {
            Ok(store) => store
                .append_event(created_event)
                .await
                .map_err(RuntimeError::from),
            Err(error) => Err(error),
        };
        let mut degradations = Vec::new();
        if let Err(error) = event_result {
            record_post_commit_degradation(
                &mut degradations,
                "create_note",
                note.id,
                "event_append",
                error,
            );
        }

        vector_fences.sort_by(|a, b| a.0.cmp(&b.0));
        Ok((note, embedding_report, degradations, vector_fences))
    }

    /// Search notes using a hybrid FTS5 + vector pipeline with salience weighting.
    ///
    /// Pipeline:
    /// 1. FTS5 query against `notes_<namespace>`.
    /// 2. If embedding model is configured: vector search filtered to `kind="note"`.
    /// 3. RRF fusion (k=60).
    /// 4. Salience-weighted rerank: `score *= (0.5 + 0.5 * note.salience)`.
    /// 5. Filter soft-deleted notes, apply optional kind / tag / properties predicates.
    ///    Tags and properties are pushed into the per-note fetch loop BEFORE truncation
    ///    so that matching notes ranked beyond `limit` in the raw fusion are not silently
    ///    dropped.
    /// 6. Truncate to `limit`.
    ///
    /// `tags_any`: when non-empty, only notes that have at least one of these tags
    /// (stored in `properties["tags"]`, case-insensitive match) are retained. The
    /// check happens inside the alive-note loop, before `hits.truncate(limit)`.
    ///
    /// `properties_filter`: when `Some`, only notes whose `properties` JSON object is
    /// a superset of the given filter object are retained. Also applied before truncation.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_notes(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        note_kind: Option<&str>,
        include_superseded: bool,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
    ) -> RuntimeResult<Vec<NoteSearchHit>> {
        self.search_notes_with_text_mode(
            token,
            query_text,
            query_vector,
            limit,
            note_kind,
            include_superseded,
            tags_any,
            properties_filter,
            TextQueryMode::Plain,
        )
        .await
    }

    /// Note search with an explicit lexical mode for the text arm.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_notes_with_text_mode(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        note_kind: Option<&str>,
        include_superseded: bool,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
    ) -> RuntimeResult<Vec<NoteSearchHit>> {
        let (hits, _vector_error) = self
            .search_notes_inner(
                token,
                query_text,
                query_vector,
                limit,
                note_kind,
                include_superseded,
                tags_any,
                properties_filter,
                text_mode,
                false,
            )
            .await?;
        Ok(hits)
    }

    /// Coordinator fan-out variant of [`Self::search_notes`]: the text arm
    /// still fails loud, but a vector-arm failure after a successful text leg
    /// is captured instead of discarding the text hits — mirrors
    /// [`Self::hybrid_search_outcome`]'s contract for the entity substrate.
    /// Reserved for `SubstrateCoordinator::fan_out_search_with_visibility`;
    /// every other caller keeps the fail-loud [`Self::search_notes`] contract.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_notes_outcome(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        limit: u32,
        note_kind: Option<&str>,
        include_superseded: bool,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
    ) -> RuntimeResult<NoteSearchOutcome> {
        self.search_notes_outcome_with_text_mode(
            token,
            query_text,
            limit,
            note_kind,
            include_superseded,
            tags_any,
            properties_filter,
            TextQueryMode::Plain,
        )
        .await
    }

    /// Coordinator note-search variant with an explicit lexical mode.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_notes_outcome_with_text_mode(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        limit: u32,
        note_kind: Option<&str>,
        include_superseded: bool,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
        text_mode: TextQueryMode,
    ) -> RuntimeResult<NoteSearchOutcome> {
        let (hits, vector_error) = self
            .search_notes_inner(
                token,
                query_text,
                None,
                limit,
                note_kind,
                include_superseded,
                tags_any,
                properties_filter,
                text_mode,
                true,
            )
            .await?;
        Ok(NoteSearchOutcome { hits, vector_error })
    }

    /// Restore a note tombstone owned by the caller's primary namespace.
    ///
    /// A live note holding the tombstone's `(namespace, kind, key)` refuses
    /// the operation before any row changes. The same condition is repeated
    /// in the guarded restore statement for the concurrent race.
    pub async fn restore_note(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<(Note, bool)>> {
        let Some(note) = self.notes(token)?.get_note_including_deleted(id).await? else {
            return Ok(None);
        };
        if note.namespace != token.namespace().as_str() {
            return Ok(None);
        }
        if note.deleted_at.is_none() {
            return Ok(Some((note, false)));
        }
        if let Some(key) = note.key.as_deref() {
            if let Some(holder) = self
                .notes(token)?
                .get_live_notes_by_key(&note.namespace, key, Some(&note.kind))
                .await?
                .into_iter()
                .find(|holder| holder.id != note.id)
            {
                return Err(restore_key_conflict(key, &holder));
            }
        }
        let updated_at =
            Utc::now()
                .timestamp_micros()
                .max(note.updated_at.checked_add(1).ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "note {id} updated_at is already at i64::MAX and cannot advance"
                    ))
                })?);
        let mut params = vec![
            SqlValue::Text("active".into()),
            SqlValue::Integer(updated_at),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(note.namespace.clone()),
            SqlValue::Text(note.kind.clone()),
        ];
        let key_clause = if let Some(key) = note.key.as_deref() {
            params.push(SqlValue::Text(key.to_owned()));
            format!(
                " AND (key IS NULL OR NOT EXISTS (SELECT 1 FROM notes live \
                          WHERE live.namespace=?4 AND live.kind=?5 AND live.key=?{} \
                            AND live.deleted_at IS NULL AND live.id != notes.id))",
                params.len()
            )
        } else {
            String::new()
        };
        let mut restored = note.clone();
        restored.status = "active".into();
        restored.deleted_at = None;
        restored.updated_at = updated_at;
        restored.version = restored
            .version
            .checked_add(1)
            .ok_or_else(|| RuntimeError::Internal(format!("note {id} version is exhausted")))?;
        let mut statements = vec![PlanStatement {
            statement: SqlStatement {
                sql: format!(
                    "UPDATE notes SET status=?1, deleted_at=NULL, updated_at=?2 \
                     WHERE id=?3 AND namespace=?4 AND kind=?5 AND deleted_at IS NOT NULL{key_clause}"
                ),
                params,
                label: Some("note-restore".into()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }];
        // Text index published in the same unit as the row; see restore_entity.
        for statement in
            khive_db::stores::text::delete_document_statements("fts_notes", &restored.namespace, id)
                .into_iter()
                .chain(insert_document_statements(
                    "fts_notes",
                    &note_fts_document(&restored),
                ))
        {
            statements.push(PlanStatement {
                statement,
                guard: None,
            });
        }
        let plan = AtomicOpPlan::Update(Box::new(UpdatePlan {
            graph_effects: Vec::new(),
            target_id: id,
            statements,
            post_commit: PostCommitEffect::None,
            edge_natural_key: None,
            idempotent_noop: false,
            entity_guard: None,
            note_guard: None,
            note_vector_purge: None,
            note_embedding_inheritance: None,
        }));
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                #[cfg(any(test, feature = "fault-injection"))]
                if consume_fault(&FTS_FAIL_NS, &restored.namespace) {
                    return Err(restore_reindex_failed(
                        "note",
                        id,
                        RuntimeError::Internal("injected FTS failure".to_string()),
                    ));
                }
                let reindexed = self.reindex_note_with_report(token, &restored).await;
                let report = reindexed.map_err(|e| restore_reindex_failed("note", id, e))?;
                let degradations = report.post_commit_degradations();
                legacy_post_commit_result("restore_note", id, Some((restored, true)), degradations)
            }
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::GuardFailed { .. },
                ..
            }) => {
                if let Some(key) = note.key.as_deref() {
                    if let Some(holder) = self
                        .notes(token)?
                        .get_live_notes_by_key(&note.namespace, key, Some(&note.kind))
                        .await?
                        .into_iter()
                        .find(|holder| holder.id != note.id)
                    {
                        return Err(restore_key_conflict(key, &holder));
                    }
                }
                Err(RuntimeError::NotFound(format!(
                    "note {id} is no longer a caller-owned tombstone"
                )))
            }
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("note restore rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }
}

/// Fully specified edge creation request — input to [`KhiveRuntime::build_edge`]
/// and [`KhiveRuntime::link_many`].
#[derive(Clone, Debug)]
pub struct LinkSpec {
    pub namespace: Option<String>,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub metadata: Option<serde_json::Value>,
    pub resurrect: bool,
}

/// Fully specified entity creation request — input to [`KhiveRuntime::create_many`].
///
/// `entity_type` is validated at the runtime layer by the pack-installed
/// entity-type validator. When a validator
/// is installed (e.g. by `KgPack`), unknown types are rejected with the valid
/// set listed. When no validator is installed (bare runtime without packs),
/// the value passes through — the handler layer is the primary enforcement point.
#[derive(Clone, Debug)]
pub struct EntityCreateSpec {
    pub kind: String,
    pub entity_type: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub properties: Option<serde_json::Value>,
    pub tags: Vec<String>,
}

// INLINE TEST JUSTIFICATION: tests here exercise private helpers (canonical_edge_endpoints,
// validate_edge_metadata, merge_dependency_kind, link-fail injection) and runtime methods
// that require pub(crate) KhiveRuntime construction. Moving them to tests/ would require
// pub-exporting those private helpers, which would widen the crate's public API surface
// undesirably. Broad behavioral tests live in tests/integration.rs.
#[cfg(test)]
#[path = "operations_tests.rs"]
mod tests;
