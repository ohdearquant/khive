// See docs/operations.md#module-layout.
//! High-level operations composing storage capabilities into user-facing verbs.
//!
//! Fault-injection arming uses scoped guards only — see docs/operations.md#fault-injection-arm-migration.

use std::collections::HashMap;
use std::str::FromStr;

use chrono::Utc;
use serde::Serialize;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::entity::EntityTypeCounts;
use khive_storage::graph::{CommitAnnotationGuard, CommitAnnotationInsertOutcome};
use khive_storage::note::Note;
use khive_storage::types::{
    DeleteMode, DirectedNeighborHit, Direction, EdgeSortField, EdgeUpsertDisposition,
    EdgeUpsertRefusal, EdgeUpsertRequest, EdgeUpsertResult, GraphPath, GuardedEdgeUpsertOutcome,
    LinkId, NeighborCursor, NeighborHit, NeighborQuery, Page, PageRequest, SeekCursor, SortOrder,
    SqlRow, SqlStatement, SqlValue, TextFilter, TextQueryMode, TextSearchRequest, TraversalRequest,
};
use khive_storage::{
    Attachment, AttachmentSubstrate, Edge, EdgeRelation, Entity, EntityFilter, Event, EventFilter,
    NewAttachment,
};
use khive_types::{EdgeEndpointRule, EndpointKind, EventKind, KhiveError, SubstrateKind};

use khive_db::stores::entity::{entity_hard_delete_statement, entity_upsert_statement};
use khive_db::stores::event::hard_delete_lineage_warning_statements;
use khive_db::stores::graph::{
    compose_graph_mutation_events, edge_hard_delete_statement, purge_incident_edges_statement,
    GraphMutationOutcome, GraphMutationPreconditions, GraphMutationRequest,
};
use khive_db::stores::note::note_hard_delete_statement;
use khive_db::stores::text::insert_document_statements;
use khive_db::{pool::RuntimeWriteOperation, SqliteError};
use rusqlite::OptionalExtension;

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
    AddEntityPlan, AffectedRowGuard, DeletePlan, PlanStatement, PostCommitEffect, UpdatePlan,
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
fn legacy_post_commit_result<T>(
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

#[cfg(any(test, feature = "fault-injection"))]
mod fault_injection;

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

/// A note search result with UUID, salience-weighted RRF score, and display text.
#[derive(Clone, Debug)]
pub struct NoteSearchHit {
    pub note_id: Uuid,
    pub score: DeterministicScore,
    pub rank_score_kind: crate::RankScoreKind,
    pub signals: crate::SearchSignals,
    pub source: crate::SearchSource,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

fn salience_weighted_rank(score: DeterministicScore, salience: Option<f64>) -> DeterministicScore {
    const SCALE_RAW: i128 = 1_i128 << 32;
    let salience = DeterministicScore::from_f64(salience.unwrap_or(0.5));
    let weight_raw = SCALE_RAW / 2 + i128::from(salience.to_raw()) / 2;
    // Match khive-score's fixed-point multiplication and saturation without
    // converting the derived weight or ranking score back to floating point.
    let weighted_raw = i128::from(score.to_raw()) * weight_raw / SCALE_RAW;
    DeterministicScore::from_raw(weighted_raw.clamp(
        i128::from(DeterministicScore::NEG_INF.to_raw()),
        i128::from(DeterministicScore::MAX.to_raw()),
    ) as i64)
}

/// Result of [`KhiveRuntime::search_notes_outcome`]: the fused hits — text
/// hits alone when the vector arm failed — plus the vector arm's error, if
/// any. Mirrors [`crate::HybridSearchOutcome`] for the note substrate.
#[derive(Clone, Debug)]
pub struct NoteSearchOutcome {
    pub hits: Vec<NoteSearchHit>,
    pub vector_error: Option<String>,
}

/// Re-insert hyphens at canonical UUID positions (8-4-4-4-12) into a
/// hyphen-free hex prefix, so a `LIKE '<pattern>%'` scan against the
/// hyphenated `id` column matches correctly. Prefixes that already
/// contain a hyphen are passed through unchanged. No-op for len <= 8
/// (already correct). Input longer than 32 hex chars is NOT truncated: the
/// extra hex chars are appended past the canonical 12-char final segment
/// with no further hyphen, so the resulting `LIKE` pattern requires literal
/// characters beyond position 36 that no real (36-char) UUID string can
/// ever have — the scan naturally fails closed instead of silently
/// resolving `<valid-32-hex><extra-hex>` to the valid UUID.
pub fn hex_prefix_to_uuid_pattern(prefix: &str) -> String {
    if prefix.contains('-') {
        return prefix.to_string();
    }
    const BOUNDARIES: [usize; 4] = [8, 13, 18, 23]; // post-hyphen-insertion offsets
    let mut out = String::with_capacity(36);
    for c in prefix.chars() {
        if BOUNDARIES.contains(&out.len()) {
            out.push('-');
        }
        out.push(c);
    }
    out
}

/// Return the inclusive lower and exclusive upper bounds for a UUID prefix
/// stored as a canonical lowercase UUID string under SQLite's BINARY collation.
///
/// Compact hexadecimal prefixes and prefixes of the canonical dashed spelling
/// are accepted. The returned bounds are canonicalized to lowercase; malformed
/// dashed spellings and prefixes longer than one UUID fail closed. The upper
/// bound is the shortest lexicographic successor, so a trailing run of `f`
/// digits carries into the preceding digit. `g` is the exclusive sentinel when
/// the prefix is all `f`, because canonical UUID strings contain only `0`-`f`.
pub fn uuid_prefix_bounds(prefix: &str) -> Option<(String, String)> {
    const HYPHEN_POSITIONS: [usize; 4] = [8, 13, 18, 23];

    let compact = if prefix.contains('-') {
        if prefix.len() > 36 {
            return None;
        }
        let mut compact = String::with_capacity(32);
        for (index, byte) in prefix.bytes().enumerate() {
            if HYPHEN_POSITIONS.contains(&index) {
                if byte != b'-' {
                    return None;
                }
            } else if byte.is_ascii_hexdigit() {
                compact.push(char::from(byte.to_ascii_lowercase()));
            } else {
                return None;
            }
        }
        compact
    } else {
        if prefix.is_empty()
            || prefix.len() > 32
            || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        prefix.to_ascii_lowercase()
    };

    if compact.is_empty() || compact.len() > 32 {
        return None;
    }

    let lower = hex_prefix_to_uuid_pattern(&compact);
    let mut successor = compact.into_bytes();
    let mut carried_past_start = true;
    for index in (0..successor.len()).rev() {
        let next = match successor[index] {
            b'0'..=b'8' | b'a'..=b'e' => Some(successor[index] + 1),
            b'9' => Some(b'a'),
            b'f' => None,
            _ => return None,
        };
        if let Some(next) = next {
            successor[index] = next;
            successor.truncate(index + 1);
            carried_past_start = false;
            break;
        }
    }

    let upper = if carried_past_start {
        "g".to_string()
    } else {
        let compact_upper = String::from_utf8(successor).ok()?;
        hex_prefix_to_uuid_pattern(&compact_upper)
    };
    Some((lower, upper))
}

fn resolve_prefix_statement(
    table: &str,
    has_deleted_at: bool,
    include_deleted: bool,
    namespaces: Option<&[String]>,
    lower: &str,
    upper: &str,
) -> SqlStatement {
    let namespace_clause = namespaces.map(|namespaces| {
        let placeholders: Vec<String> = (0..namespaces.len())
            .map(|index| format!("?{}", index + 3))
            .collect();
        format!(" AND namespace IN ({})", placeholders.join(", "))
    });
    let deleted_filter = if has_deleted_at && !include_deleted {
        " AND deleted_at IS NULL"
    } else {
        ""
    };
    let mut params = vec![
        SqlValue::Text(lower.to_owned()),
        SqlValue::Text(upper.to_owned()),
    ];
    if let Some(namespaces) = namespaces {
        params.extend(
            namespaces
                .iter()
                .map(|namespace| SqlValue::Text(namespace.clone())),
        );
    }

    SqlStatement {
        sql: format!(
            "SELECT id FROM {table} \
             WHERE id >= ?1 AND id < ?2{namespace_clause}{deleted_filter} ORDER BY id LIMIT 2",
            namespace_clause = namespace_clause.as_deref().unwrap_or("")
        ),
        params,
        label: Some("resolve_prefix".into()),
    }
}

fn text_preview(text: &str, max_chars: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(max_chars).collect())
    }
}

/// Symmetric relations (`competes_with`, `composed_with`) are stored with a
/// canonical source (lower UUID wins), so a directed `Out` or `In` query may
/// miss results. When the relations filter is non-empty and contains **only**
/// symmetric relations, override direction to `Both` so callers always see all
/// edges for these relations regardless of storage canonicalization.
fn normalize_symmetric_direction(
    direction: Direction,
    relations: Option<&[EdgeRelation]>,
) -> Direction {
    let Some(rels) = relations else {
        return direction;
    };
    if rels.is_empty() {
        return direction;
    }
    let all_symmetric = rels
        .iter()
        .all(|r| matches!(r, EdgeRelation::CompetesWith | EdgeRelation::ComposedWith));
    if all_symmetric {
        Direction::Both
    } else {
        direction
    }
}

/// Stable tie-break rank for [`Direction`] — `Out` before `In` — used to make
/// the both-direction sort/dedup key total over self-loop edges. A self-loop
/// (`source_id == target_id == node_id`) produces two `UNION ALL` rows with
/// the same `(node_id, edge_id)` but opposite directions; without direction in
/// the key, sort-then-dedup collapses them to one and drops the direction
/// parity a separate `Out` call plus a separate `In` call would preserve.
fn direction_sort_rank(direction: &Direction) -> u8 {
    match direction {
        Direction::Out => 0,
        Direction::In => 1,
        Direction::Both => 2,
    }
}

fn note_title(note: &Note) -> Option<String> {
    note.name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(format!("[{}]", note.kind.as_str())))
}

fn note_snippet(note: &Note) -> Option<String> {
    text_preview(&note.content, 200)
}

/// Result of resolving a UUID to its substrate kind.
#[derive(Clone, Debug)]
pub enum Resolved {
    Entity(Entity),
    Note(Note),
    Event(Event),
    /// A record owned by a pack's private tables.
    ///
    /// `pack` identifies the owning pack by name, `kind` is the pack-local
    /// record type (e.g. "domain", "atom"), and `data` is the full record as
    /// a JSON Value. Pack-private records are not valid edge endpoints,
    /// annotates sources, or task context entities.
    PackRecord {
        pack: String,
        kind: String,
        data: serde_json::Value,
    },
}

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

/// `true` if `spec` matches the given substrate + kind + entity_type triple.
///
/// Pure and DB-free — exposed so offline consumers (e.g. `kkernel kg
/// validate`, which parses `(substrate, kind, entity_type)` straight out of
/// NDJSON with no live record to resolve) can apply the exact same
/// `EdgeEndpointRule` matching semantics `pack_rule_allows` uses internally,
/// instead of re-deriving a parallel matcher that could drift out of sync.
pub fn endpoint_matches(
    spec: &EndpointKind,
    substrate: &str,
    kind: &str,
    entity_type: Option<&str>,
) -> bool {
    match spec {
        EndpointKind::EntityOfKind(k) => substrate == "entity" && *k == kind,
        EndpointKind::NoteOfKind(k) => substrate == "note" && *k == kind,
        EndpointKind::EntityOfType {
            kind: k,
            entity_type: t,
        } => substrate == "entity" && *k == kind && entity_type == Some(*t),
    }
}

/// `true` if `spec` matches the given substrate + kind + entity_type triple,
/// treating an *absent* `entity_type` on the query side as unconstrained
/// rather than an exact match against "no subtype".
///
/// Used only by the static GQL impossibility hint (`static_impossible_edge_pattern_warnings`,
/// `accepted_entity_kind_pairs_for_relation`), which reasons over a *pattern*
/// endpoint, not a resolved entity. A pattern endpoint that names a kind but
/// no `entity_type` (`(a:concept)-[:depends_on]->(b:concept)`) has not ruled
/// out any subtype, so an `EntityOfType` rule for that kind still makes the
/// triple possible — unlike `endpoint_matches`, which the live link
/// validator applies to *resolved* entities, where a `None` `entity_type`
/// means the entity genuinely has no subtype and must be an exact miss
/// against a typed rule. Do not use this for validation.
fn pattern_endpoint_matches(
    spec: &EndpointKind,
    substrate: &str,
    kind: &str,
    entity_type: Option<&str>,
) -> bool {
    match spec {
        EndpointKind::EntityOfType {
            kind: k,
            entity_type: t,
        } => substrate == "entity" && *k == kind && entity_type.is_none_or(|et| et == *t),
        _ => endpoint_matches(spec, substrate, kind, entity_type),
    }
}

/// Relations that a composed pack `EDGE_RULES` set accepts for a given
/// `(entity_kind, entity_type)` endpoint pair, using the EXACT SAME
/// `endpoint_matches` semantics `pack_rule_allows` applies internally
/// (`EntityOfKind`, `EntityOfType`, `NoteOfKind`) — never a re-filtered copy.
///
/// Both endpoints are treated as entities (substrate `"entity"`), matching
/// the only case pack-layer error-hint code needs (issue #543): a rejected
/// `link` between two already-resolved entities. `entity_type` is the
/// pack-owned granular subtype (e.g. `"theorem"`); pass `None` for
/// untyped entities. Exposed so `khive-pack-kg`'s hint derivation cannot
/// silently diverge from the validator by only matching `EntityOfKind` and
/// missing pack rules declared via `EntityOfType` (e.g. `khive-pack-formal`'s
/// typed `theorem -> definition` `depends_on` rules).
pub fn accepted_pack_relations_for_entities(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<EdgeRelation> {
    let mut relations: Vec<EdgeRelation> = rules
        .iter()
        .filter(|r| {
            endpoint_matches(&r.source, "entity", src_kind, src_entity_type)
                && endpoint_matches(&r.target, "entity", tgt_kind, tgt_entity_type)
        })
        .map(|r| r.relation)
        .collect();
    relations.sort_by_key(|r| r.as_str());
    relations.dedup();
    relations
}

/// Relations accepted for one resolved entity endpoint pair under the full
/// live contract: the base allowlist plus the loaded packs' additive rules.
///
/// This is the pair-oriented counterpart to the private
/// `accepted_entity_kind_pairs_for_relation` helper. It is shared by validation
/// errors and pack-layer hints so every write path can tell a caller which
/// relations would be legal without maintaining a second endpoint table.
/// Pack declarations for relations with dedicated substrate branches are
/// excluded because the live validator resolves `annotates` and the three
/// same-substrate special relations before pack rules are consulted.
pub fn accepted_entity_relations_for_entities(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<EdgeRelation> {
    let mut relations: Vec<EdgeRelation> = BASE_ENTITY_ENDPOINT_RULES
        .iter()
        .filter(|(src, _relation, tgt)| (*src == "*" || *src == src_kind) && *tgt == tgt_kind)
        .map(|(_src, relation, _tgt)| *relation)
        .collect();
    relations.extend(
        accepted_pack_relations_for_entities(
            rules,
            src_kind,
            src_entity_type,
            tgt_kind,
            tgt_entity_type,
        )
        .into_iter()
        .filter(|relation| {
            *relation != EdgeRelation::Annotates && !crate::pack::is_special_relation(*relation)
        }),
    );
    relations.sort_by_key(|relation| relation.as_str());
    relations.dedup();
    relations
}

fn accepted_entity_relations_description(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> String {
    let relations = accepted_entity_relations_for_entities(
        rules,
        src_kind,
        src_entity_type,
        tgt_kind,
        tgt_entity_type,
    );
    if relations.is_empty() {
        "none".to_string()
    } else {
        relations
            .iter()
            .map(EdgeRelation::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Hint-only counterpart to [`accepted_pack_relations_for_entities`] that
/// matches via [`pattern_endpoint_matches`] instead of [`endpoint_matches`],
/// so an absent `entity_type` is treated as unconstrained rather than an
/// exact-match miss against `EntityOfType` rules. Used exclusively by the
/// static GQL impossibility hint — never by validation.
fn accepted_pack_relations_for_pattern_entities(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<EdgeRelation> {
    let mut relations: Vec<EdgeRelation> = rules
        .iter()
        .filter(|r| {
            pattern_endpoint_matches(&r.source, "entity", src_kind, src_entity_type)
                && pattern_endpoint_matches(&r.target, "entity", tgt_kind, tgt_entity_type)
        })
        .map(|r| r.relation)
        .collect();
    relations.sort_by_key(|r| r.as_str());
    relations.dedup();
    relations
}

/// All `(source_kind, target_kind)` entity-kind pairs — restricted to the closed
/// 8-kind base [`khive_types::EntityKind`] taxonomy — that accept `relation`
/// under the composed base allowlist plus pack `EDGE_RULES`. Reuses
/// [`base_entity_rule_allows`] and [`accepted_pack_relations_for_pattern_entities`]
/// over the closed kind set rather than re-deriving a parallel table (for GQL
/// query-pattern hint derivation). Pack rules are skipped when
/// `crate::pack::is_special_relation` is true (supersedes/supports/refutes):
/// those relations are resolved by the live validator's special-relation branch
/// before `pack_rule_allows` is ever reached — see `pack.rs`'s
/// `edge_endpoint_table` doc comment.
fn accepted_entity_kind_pairs_for_relation(
    pack_rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
) -> Vec<(&'static str, &'static str)> {
    let mut pairs = Vec::new();
    for src in khive_types::EntityKind::ALL {
        for tgt in khive_types::EntityKind::ALL {
            let allowed = base_entity_rule_allows(src.name(), relation, tgt.name())
                || (!crate::pack::is_special_relation(relation)
                    && accepted_pack_relations_for_pattern_entities(
                        pack_rules,
                        src.name(),
                        None,
                        tgt.name(),
                        None,
                    )
                    .contains(&relation));
            if allowed {
                pairs.push((src.name(), tgt.name()));
            }
        }
    }
    pairs
}

/// Scans a GQL `MATCH` pattern for edges that name an explicit relation and
/// explicit entity kinds on both endpoints — a single mandatory hop, a fixed
/// direction — where the `(source_kind, relation, target_kind)` triple can
/// never match under the composed edge endpoint contract. Returns one warning
/// per statically-impossible edge.
///
/// Deliberately conservative: unlabeled nodes, unlabeled/multi-relation edges,
/// undirected edges, variable-length hops, and note-kind endpoints are left
/// unchecked, since none of those name a single static triple to test against
/// the validator (issue #593).
///
/// Mirrors the validator's special-relation precedence for `supersedes` /
/// `supports` / `refutes` (see [`accepted_entity_kind_pairs_for_relation`]):
/// pack rules never make those triples possible, only the base allowlist does.
fn static_impossible_edge_pattern_warnings(
    language: khive_query::QueryLanguage,
    pattern: &khive_query::ast::MatchPattern,
    pack_rules: &[EdgeEndpointRule],
) -> Vec<String> {
    use khive_query::ast::{EdgeDirection, PatternElement};

    if language != khive_query::QueryLanguage::Gql {
        return Vec::new();
    }

    let elements = &pattern.elements;
    let mut warnings = Vec::new();

    for (i, el) in elements.iter().enumerate() {
        let PatternElement::Edge(edge) = el else {
            continue;
        };
        if edge.relations.len() != 1 || edge.min_hops != 1 || edge.max_hops != 1 {
            continue;
        }
        let (left, right) = match (elements.get(i.wrapping_sub(1)), elements.get(i + 1)) {
            (Some(PatternElement::Node(l)), Some(PatternElement::Node(r))) => (l, r),
            _ => continue,
        };
        let (src_node, tgt_node) = match edge.direction {
            EdgeDirection::Out => (left, right),
            EdgeDirection::In => (right, left),
            EdgeDirection::Both => continue,
        };
        let (Some(src_raw), Some(tgt_raw)) = (src_node.kind.as_deref(), tgt_node.kind.as_deref())
        else {
            continue;
        };
        let (Ok(src_kind), Ok(tgt_kind)) = (
            src_raw.parse::<khive_types::EntityKind>(),
            tgt_raw.parse::<khive_types::EntityKind>(),
        ) else {
            continue;
        };
        let Ok(relation) = edge.relations[0].parse::<EdgeRelation>() else {
            continue;
        };

        let possible = base_entity_rule_allows(src_kind.name(), relation, tgt_kind.name())
            || (!crate::pack::is_special_relation(relation)
                && accepted_pack_relations_for_pattern_entities(
                    pack_rules,
                    src_kind.name(),
                    src_node.entity_type.as_deref(),
                    tgt_kind.name(),
                    tgt_node.entity_type.as_deref(),
                )
                .contains(&relation));
        if possible {
            continue;
        }

        let accepted = accepted_entity_kind_pairs_for_relation(pack_rules, relation);
        let accepted_str = if accepted.is_empty() {
            "none".to_string()
        } else {
            accepted
                .iter()
                .map(|(s, t)| format!("{s}->{t}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        warnings.push(format!(
            "pattern ({src})-[:{relation}]->({tgt}) can never match: '{relation}' does not accept \
             {src}->{tgt} endpoints; accepted source->target kinds for '{relation}': {accepted_str}",
            src = src_kind.name(),
            tgt = tgt_kind.name(),
        ));
    }

    warnings
}

/// `true` if any pack-declared edge endpoint rule allows the
/// `(source, relation, target)` triple. Pack rules are additive only.
fn pack_rule_allows(
    rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
    src: Option<&Resolved>,
    tgt: Option<&Resolved>,
) -> bool {
    let Some((src_sub, src_kind, src_type)) = resolved_pair(src) else {
        return false;
    };
    let Some((tgt_sub, tgt_kind, tgt_type)) = resolved_pair(tgt) else {
        return false;
    };
    rules.iter().any(|r| {
        r.relation == relation
            && endpoint_matches(&r.source, src_sub, src_kind, src_type)
            && endpoint_matches(&r.target, tgt_sub, tgt_kind, tgt_type)
    })
}

/// Base entity endpoint allowlist — the closed set of permitted entity→entity
/// relation triples.
///
/// Each entry `(src_kind, relation, tgt_kind)` explicitly allows that combination.
/// `"*"` as `src_kind` means "any entity kind" (used by `instance_of` whose source
/// is unrestricted).
///
/// Pack rules (via `EDGE_RULES`) are additive — they cannot remove rows here.
/// Exposed via `base_entity_endpoint_rules()` for the ADR-076 certificate tests.
pub const BASE_ENTITY_ENDPOINT_RULES: &[(&str, EdgeRelation, &str)] = &[
    // Structure
    ("concept", EdgeRelation::Contains, "concept"),
    ("project", EdgeRelation::Contains, "project"),
    ("project", EdgeRelation::Contains, "artifact"),
    ("org", EdgeRelation::Contains, "project"),
    ("org", EdgeRelation::Contains, "service"),
    ("concept", EdgeRelation::PartOf, "concept"),
    ("project", EdgeRelation::PartOf, "project"),
    ("project", EdgeRelation::PartOf, "org"),
    ("*", EdgeRelation::InstanceOf, "concept"),
    ("service", EdgeRelation::InstanceOf, "project"),
    // ADR-002 amendment (ADR-191): web hyperlink — a document points at
    // another document it links to. No qualifier inference (unlike
    // depends_on); the endpoint pair is intentionally narrow (document only,
    // no service/concept targets — see ADR-191 D2/F10).
    ("document", EdgeRelation::LinksTo, "document"),
    // ADR-196 location, amended 2026-10-05: the source occupies or is manifested in the target
    // without being a constituent of it; base rows for concept and org, packs narrow the rest.
    ("concept", EdgeRelation::LocatedIn, "concept"),
    ("org", EdgeRelation::LocatedIn, "concept"),
    // Ownership
    ("person", EdgeRelation::Owns, "org"),
    ("org", EdgeRelation::Owns, "org"),
    // Derivation
    ("concept", EdgeRelation::Extends, "concept"),
    ("concept", EdgeRelation::VariantOf, "concept"),
    ("artifact", EdgeRelation::VariantOf, "artifact"),
    ("concept", EdgeRelation::IntroducedBy, "document"),
    ("concept", EdgeRelation::IntroducedBy, "person"),
    ("artifact", EdgeRelation::IntroducedBy, "document"),
    ("project", EdgeRelation::IntroducedBy, "document"),
    // ADR-002 amendment (ADR-167): service provenance — the document that
    // introduced a service (its ADR or design record).
    ("service", EdgeRelation::IntroducedBy, "document"),
    ("document", EdgeRelation::IntroducedBy, "person"),
    ("document", EdgeRelation::IntroducedBy, "org"),
    ("concept", EdgeRelation::IntroducedBy, "org"),
    // Provenance
    ("artifact", EdgeRelation::DerivedFrom, "dataset"),
    ("artifact", EdgeRelation::DerivedFrom, "document"),
    ("artifact", EdgeRelation::DerivedFrom, "project"),
    ("artifact", EdgeRelation::DerivedFrom, "artifact"),
    // ADR-002 amendment 2026-07-27: publication provenance — a curated or
    // filtered publication copy points at the canonical source document.
    ("document", EdgeRelation::DerivedFrom, "document"),
    // Temporal
    ("document", EdgeRelation::Precedes, "document"),
    ("dataset", EdgeRelation::Precedes, "dataset"),
    ("artifact", EdgeRelation::Precedes, "artifact"),
    ("service", EdgeRelation::Precedes, "service"),
    ("project", EdgeRelation::Precedes, "project"),
    // Dependency
    ("project", EdgeRelation::DependsOn, "project"),
    ("service", EdgeRelation::DependsOn, "project"),
    ("service", EdgeRelation::DependsOn, "service"),
    ("service", EdgeRelation::DependsOn, "artifact"),
    ("service", EdgeRelation::DependsOn, "dataset"),
    ("artifact", EdgeRelation::DependsOn, "project"),
    ("artifact", EdgeRelation::DependsOn, "service"),
    ("document", EdgeRelation::DependsOn, "document"),
    ("concept", EdgeRelation::Enables, "concept"),
    ("service", EdgeRelation::Enables, "concept"),
    ("dataset", EdgeRelation::Enables, "concept"),
    // Implementation
    ("project", EdgeRelation::Implements, "concept"),
    ("service", EdgeRelation::Implements, "concept"),
    // Lateral
    ("concept", EdgeRelation::CompetesWith, "concept"),
    ("project", EdgeRelation::CompetesWith, "project"),
    ("service", EdgeRelation::CompetesWith, "service"),
    ("org", EdgeRelation::CompetesWith, "org"),
    ("concept", EdgeRelation::ComposedWith, "concept"),
    ("project", EdgeRelation::ComposedWith, "project"),
    // Versioning (Supersedes — Concept/Document/Artifact/Service/Dataset only)
    ("concept", EdgeRelation::Supersedes, "concept"),
    ("document", EdgeRelation::Supersedes, "document"),
    ("artifact", EdgeRelation::Supersedes, "artifact"),
    ("service", EdgeRelation::Supersedes, "service"),
    ("dataset", EdgeRelation::Supersedes, "dataset"),
    // Epistemic (Supports/Refutes — evidence sources → Concept claim only)
    ("concept", EdgeRelation::Supports, "concept"),
    ("document", EdgeRelation::Supports, "concept"),
    ("dataset", EdgeRelation::Supports, "concept"),
    ("artifact", EdgeRelation::Supports, "concept"),
    ("concept", EdgeRelation::Refutes, "concept"),
    ("document", EdgeRelation::Refutes, "concept"),
    ("dataset", EdgeRelation::Refutes, "concept"),
    ("artifact", EdgeRelation::Refutes, "concept"),
];

/// Returns the base entity endpoint allowlist.
///
/// The returned slice is the same data that `base_entity_rule_allows` consults at
/// runtime. Exposed for the ADR-076 certificate tests in `khive-pack-kg`, which
/// must audit live rules rather than hand-copied snapshots.
pub fn base_entity_endpoint_rules() -> &'static [(&'static str, EdgeRelation, &'static str)] {
    BASE_ENTITY_ENDPOINT_RULES
}

/// `true` if `(src_kind, relation, tgt_kind)` is in the base entity endpoint
/// allowlist. Pure and DB-free — exposed alongside [`base_entity_endpoint_rules`]
/// so offline consumers (e.g. `kkernel kg validate`) can apply the exact same
/// base-table membership test the live validator uses, instead of re-deriving
/// a parallel `.any()` predicate over a hand-copied allowlist.
pub fn base_entity_rule_allows(src_kind: &str, relation: EdgeRelation, tgt_kind: &str) -> bool {
    BASE_ENTITY_ENDPOINT_RULES.iter().any(|(src, rel, tgt)| {
        *rel == relation && (*src == "*" || *src == src_kind) && *tgt == tgt_kind
    })
}

/// Canonical endpoint order for symmetric relations (F012).
///
/// For `competes_with` and `composed_with`, normalises direction so that
/// `source_uuid < target_uuid` (lexicographic on the UUID bytes). This
/// collapses A→B and B→A into a single canonical row, preventing duplicates.
pub(crate) fn canonical_edge_endpoints(
    relation: EdgeRelation,
    source_id: Uuid,
    target_id: Uuid,
) -> (Uuid, Uuid) {
    relation.canonical_endpoints(source_id, target_id)
}

/// Keep endpoint substrates paired with their IDs when a symmetric link swaps direction.
pub(crate) fn canonical_edge_endpoint_kinds(
    requested_source_id: Uuid,
    canonical_source_id: Uuid,
    source_kind: EdgeEndpointKind,
    target_kind: EdgeEndpointKind,
) -> (EdgeEndpointKind, EdgeEndpointKind) {
    if requested_source_id == canonical_source_id {
        (source_kind, target_kind)
    } else {
        (target_kind, source_kind)
    }
}

/// Infer the default `dependency_kind` from endpoint entity kinds.
///
/// `pub(crate)` so `crate::atomic_prepare::prepare_link` can reuse this exact
/// inference table, keeping `--atomic link` byte-for-byte consistent with the
/// non-atomic `link()` rather than re-deriving the table.
pub(crate) fn infer_dependency_kind(src_kind: &str, tgt_kind: &str) -> Option<&'static str> {
    match (src_kind, tgt_kind) {
        ("project", "project") => Some("build"),
        ("service", "service") => Some("runtime"),
        ("service", "dataset") => Some("data"),
        ("service", "artifact") => Some("artifact"),
        ("artifact", "project") | ("artifact", "service") => Some("tooling"),
        ("document", "document") => Some("normative"),
        _ => None,
    }
}

/// Merge an inferred `dependency_kind` into `depends_on` edge metadata.
///
/// If `metadata` already carries a `dependency_kind` key the existing value is
/// preserved. If the key is absent and the endpoint pair has a known default,
/// the inferred value is added. Returns `metadata` unchanged for all other
/// cases (no matching default, or metadata already has the key).
///
/// `pub(crate)` so `crate::atomic_prepare::prepare_link` can reuse it for
/// atomic/non-atomic parity.
pub(crate) fn merge_dependency_kind(
    src_kind: &str,
    tgt_kind: &str,
    metadata: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    // JSON null has the same meaning as an omitted metadata argument. All
    // callers validate the object shape before reaching inference.
    let metadata = metadata.filter(|value| !value.is_null());
    if let Some(ref m) = metadata {
        if m.get("dependency_kind").is_some() {
            return metadata;
        }
    }
    let Some(inferred) = infer_dependency_kind(src_kind, tgt_kind) else {
        return metadata;
    };
    let mut obj = metadata.unwrap_or_else(|| serde_json::json!({}));
    if let Some(o) = obj.as_object_mut() {
        o.insert("dependency_kind".to_string(), serde_json::json!(inferred));
    }
    Some(obj)
}

/// Merge a caller-supplied top-level `dependency_kind` param into an edge's
/// `metadata` object, filling the key only if `metadata` doesn't already
/// carry one. This is distinct from `merge_dependency_kind` above (which
/// infers a default from endpoint entity kinds when no explicit value was
/// given at all) — this one folds in an EXPLICIT `dependency_kind` argument
/// the caller passed alongside `metadata`.
///
/// `pub`: the single source both `khive-pack-kg::handlers::link::handle_link`
/// (via `khive_runtime::merge_entry_metadata`) and
/// `crate::atomic_prepare::prepare_link` call. Lives in `khive-runtime` (not
/// pack-kg) because packs depend on `khive-runtime`, never the reverse: the
/// only direction that lets both call sites share one copy instead of a
/// hand-duplicated block.
pub fn merge_entry_metadata(
    metadata: Option<serde_json::Value>,
    dependency_kind: Option<String>,
) -> RuntimeResult<Option<serde_json::Value>> {
    validate_metadata_shape(metadata.as_ref())?;
    let metadata = metadata.filter(|value| !value.is_null());
    let Some(dk) = dependency_kind else {
        return Ok(metadata);
    };
    let mut obj = metadata.unwrap_or_else(|| serde_json::json!({}));
    let map = obj
        .as_object_mut()
        .ok_or_else(|| RuntimeError::InvalidInput("metadata must be a JSON object".into()))?;
    map.entry("dependency_kind".to_string())
        .or_insert_with(|| serde_json::json!(dk));
    Ok(Some(obj))
}

/// Valid `dependency_kind` values for `depends_on` edges.
const VALID_DEPENDENCY_KINDS: &[&str] = &[
    "build",
    "runtime",
    "data",
    "artifact",
    "tooling",
    "normative",
];

/// Validate that an edge weight is finite and within `[0.0, 1.0]`.
///
/// Rejects NaN, infinities, negative values, and values exceeding 1.0.
/// Used by `link` and `import_kg` to enforce the weight invariant consistently
/// across all edge creation paths.
pub(crate) fn validate_edge_weight(weight: f64) -> RuntimeResult<()> {
    if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
        return Err(RuntimeError::InvalidInput(format!(
            "edge weight must be finite and in [0.0, 1.0], got {weight}"
        )));
    }
    Ok(())
}

fn validate_metadata_shape(metadata: Option<&serde_json::Value>) -> RuntimeResult<()> {
    if metadata.is_some_and(|value| !value.is_null() && !value.is_object()) {
        return Err(RuntimeError::InvalidInput(
            "metadata must be a JSON object".into(),
        ));
    }
    Ok(())
}

/// Validate governed edge metadata keys.
///
/// Enforces object shape, the `depends_on` scope and vocabulary of
/// `dependency_kind`, and the boolean type of `optional`.
pub(crate) fn validate_edge_metadata(
    relation: EdgeRelation,
    metadata: Option<&serde_json::Value>,
) -> RuntimeResult<()> {
    validate_metadata_shape(metadata)?;
    let Some(meta) = metadata.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let object = meta.as_object().expect("validated metadata object");
    if object
        .get("optional")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(RuntimeError::InvalidInput(
            "metadata.optional must be a boolean".into(),
        ));
    }
    if let Some(dk) = meta.get("dependency_kind") {
        if relation != EdgeRelation::DependsOn {
            return Err(RuntimeError::InvalidInput(format!(
                "dependency_kind is only valid on depends_on edges (got {})",
                relation.as_str()
            )));
        }
        let dk_str = dk
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("dependency_kind must be a string".into()))?;
        if !VALID_DEPENDENCY_KINDS.contains(&dk_str) {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown dependency_kind {dk_str:?}; valid: {}",
                VALID_DEPENDENCY_KINDS.join(" | ")
            )));
        }
    }
    Ok(())
}

/// Returns `true` when `note_props` is a superset of all key-value pairs in `filter`.
///
/// Mirrors the semantics of `khive_pack_kg::handlers::common::props_match` so that the
/// storage-leg predicate in `search_notes` is identical to the handler-side post-filter.
fn note_props_match(note_props: Option<&serde_json::Value>, filter: &serde_json::Value) -> bool {
    let required = match filter.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return true,
    };
    let actual = match note_props.and_then(serde_json::Value::as_object) {
        Some(obj) => obj,
        None => return false,
    };
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| av == v))
}

fn note_graph_name(note: &Note) -> String {
    note.name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("[{}]", note.kind))
}

/// Collapse per-namespace `GraphPath`s from [`KhiveRuntime::traverse`] down to exactly
/// one entry per distinct `root_id`, merging by `(root_id, node_id)` (shallowest depth
/// wins), BFS-ordering the result, and re-applying `limit`. See
/// docs/operations.md#merge_traversal_paths_by_root for why naive concatenation is unsound.
fn merge_traversal_paths_by_root(paths: Vec<GraphPath>, limit: Option<u32>) -> Vec<GraphPath> {
    let mut order: Vec<Uuid> = Vec::new();
    let mut merged: HashMap<Uuid, GraphPath> = HashMap::new();
    // root_id -> (node_id -> index into merged[root_id].nodes), so a
    // shallower depth for an already-seen node updates in place instead of
    // rebuilding a seen-set from every prior namespace's contribution.
    let mut node_index: HashMap<Uuid, HashMap<Uuid, usize>> = HashMap::new();

    for path in paths {
        let existing = merged.entry(path.root_id).or_insert_with(|| {
            order.push(path.root_id);
            GraphPath {
                root_id: path.root_id,
                nodes: Vec::new(),
                total_weight: 0.0,
            }
        });
        let index = node_index.entry(path.root_id).or_default();
        for node in path.nodes {
            match index.get(&node.node_id) {
                Some(&i) => {
                    if node.depth < existing.nodes[i].depth {
                        existing.nodes[i] = node;
                    }
                }
                None => {
                    index.insert(node.node_id, existing.nodes.len());
                    existing.nodes.push(node);
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|root_id| merged.remove(&root_id))
        .map(|mut path| {
            // BFS order: ascending depth, stable within a depth.
            path.nodes.sort_by_key(|n| n.depth);
            if let Some(lim) = limit {
                let lim = lim as usize;
                let mut non_root_kept = 0usize;
                path.nodes.retain(|n| {
                    if n.depth == 0 {
                        return true;
                    }
                    if non_root_kept < lim {
                        non_root_kept += 1;
                        true
                    } else {
                        false
                    }
                });
            }
            recompute_total_weight(&mut path);
            path
        })
        .collect()
}

/// Set `total_weight` to the maximum cumulative path weight among the nodes
/// the path currently holds, matching how storage derives it for a
/// single-namespace traversal.
///
/// Call this after any edit to `nodes`. Carrying a weight across an edit is
/// what lets the field describe a node the caller was never shown: the
/// highest-weighted candidate is exactly the one a `limit` or a
/// soft-delete screen can remove while the summary keeps quoting it.
fn recompute_total_weight(path: &mut GraphPath) {
    path.total_weight = path.nodes.iter().map(|n| n.weight).fold(0.0_f64, f64::max);
}

/// Await every spawned multi-model embed task in `join_set`, returning one
/// vector per model (in model order) on full success.
///
/// `join_set` entries are `(model_index, embed_result)` — the index lets
/// completion order (which is arrival order, not spawn order) be reassembled
/// into the caller's model order. On the first failure (an embed error or a
/// task panic), every remaining handle is aborted and detached so the error
/// return is not gated on a sibling reaching a cancellation point. A sibling
/// already inside synchronous native inference may finish that call in the
/// background. Embed calls are counted when issued, before the provider
/// await, so detached completion cannot change the operation's usage count.
/// Each task owns cloned runtime/provider state and only computes an embedding;
/// storage writes remain in the parent after this drain succeeds.
async fn drain_embed_join_set<T: Send + 'static>(
    mut join_set: tokio::task::JoinSet<(usize, RuntimeResult<T>)>,
    model_count: usize,
) -> RuntimeResult<Vec<T>> {
    let mut vectors: Vec<Option<T>> = (0..model_count).map(|_| None).collect();

    while let Some(joined) = join_set.join_next().await {
        match joined {
            Ok((idx, Ok(vector))) => vectors[idx] = Some(vector),
            Ok((_idx, Err(e))) => {
                join_set.abort_all();
                return Err(e);
            }
            Err(join_err) => {
                join_set.abort_all();
                return Err(RuntimeError::Internal(format!(
                    "embed task panicked: {join_err}"
                )));
            }
        }
    }

    Ok(vectors
        .into_iter()
        .map(|v| v.expect("every model index observed exactly once by join_set drain"))
        .collect())
}

impl KhiveRuntime {
    // ---- Entity operations ----

    async fn compensate_entity_create(
        &self,
        token: &NamespaceToken,
        entity_id: Uuid,
        namespace: &str,
        vector_models: &[String],
    ) -> Vec<String> {
        let mut cleanup_errors = Vec::new();

        #[cfg(any(test, feature = "fault-injection"))]
        let entity_delete_injected = consume_fault(&ENTITY_COMPENSATION_FAIL_NS, namespace);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let entity_delete_injected = false;

        if entity_delete_injected {
            cleanup_errors.push("entity row delete: injected compensation failure".to_string());
        } else {
            match self.entities(token) {
                Ok(store) => {
                    if let Err(error) = store.delete_entity(entity_id, DeleteMode::Hard).await {
                        cleanup_errors.push(format!("entity row delete: {error}"));
                    }
                }
                Err(error) => cleanup_errors.push(format!("entity store access: {error}")),
            }
        }

        match self.text(token) {
            Ok(fts) => {
                if let Err(error) = fts.delete_document(namespace, entity_id).await {
                    cleanup_errors.push(format!("FTS document delete: {error}"));
                }
            }
            Err(error) => cleanup_errors.push(format!("FTS store access: {error}")),
        }

        for model_name in vector_models {
            match self.vectors_for_model(token, model_name) {
                Ok(vectors) => {
                    if let Err(error) = vectors.delete(entity_id).await {
                        cleanup_errors
                            .push(format!("vector delete for model {model_name}: {error}"));
                    }
                }
                Err(error) => cleanup_errors.push(format!(
                    "vector store access for model {model_name}: {error}"
                )),
            }
        }

        cleanup_errors
    }

    fn entity_create_failure(
        entity_id: Uuid,
        primary: RuntimeError,
        cleanup_errors: Vec<String>,
    ) -> RuntimeError {
        if cleanup_errors.is_empty() {
            primary
        } else {
            RuntimeError::Khive(KhiveError::internal(format!(
                "create_entity indexing failed for record {entity_id}; primary failure: \
                 {primary}; compensation failure(s): {}; partial persistence is possible; \
                 inspect and reconcile this record before retrying",
                cleanup_errors.join("; ")
            )))
        }
    }

    /// Claim a caller-derived entity id without replacing a competing row.
    /// Index writes are repeatable and never compensate by deleting the claim.
    pub async fn claim_entity_if_absent(
        &self,
        token: &NamespaceToken,
        spec: EntityClaimSpec,
    ) -> RuntimeResult<(Entity, bool)> {
        self.validate_entity_kind(&spec.kind)?;
        let entity_type =
            self.validate_entity_type_for_kind(&spec.kind, spec.entity_type.as_deref())?;
        crate::secret_gate::reject_reserved_secret_gate_property(spec.properties.as_ref())?;
        crate::secret_gate::check_at(&spec.name, "entity", "name")?;
        if let Some(description) = &spec.description {
            crate::secret_gate::check_at(description, "entity", "description")?;
        }
        if let Some(properties) = &spec.properties {
            crate::secret_gate::check_json_at(properties, "entity", "properties")?;
        }
        crate::secret_gate::check_tags_at(&spec.tags, "entity", "tags")?;

        let mut proposed = Entity::new(token.namespace().as_str(), &spec.kind, &spec.name);
        proposed.id = spec.id;
        proposed.entity_type = entity_type.clone();
        proposed.description = spec.description;
        proposed.properties = spec.properties;
        proposed.tags = spec.tags;

        let store = self.entities(token)?;
        let inserted = store.insert_entity_if_absent(proposed.clone()).await?;
        let entity = if inserted {
            proposed
        } else {
            store
                .get_entity_including_deleted(spec.id)
                .await?
                .ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "entity claim {} lost but the winning row is missing",
                        spec.id
                    ))
                })?
        };
        if entity.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "entity claim {} is soft-deleted; restore it explicitly",
                entity.id
            )));
        }
        if entity.namespace != token.namespace().as_str()
            || entity.kind != spec.kind
            || entity.entity_type.as_deref() != entity_type.as_deref()
            || !entity.name.eq_ignore_ascii_case(&spec.name)
            || !entity
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(&spec.identity_tag))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "entity claim {} belongs to a different record",
                entity.id
            )));
        }

        self.ensure_claimed_entity_create_event(token, &entity)
            .await?;
        self.reindex_claimed_entity(token, &entity).await?;
        Ok((entity, inserted))
    }

    /// A claimed row may survive a failed event append. Verify the event before
    /// a retry can report the row as registered.
    pub async fn ensure_claimed_entity_create_event(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<()> {
        if entity.namespace != token.namespace().as_str() || entity.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "entity {} is not a live row in the write namespace",
                entity.id
            )));
        }
        let events = self.events(token).map_err(|error| {
            RuntimeError::Internal(format!(
                "entity {} persists but its create event store is unavailable: {error}",
                entity.id
            ))
        })?;
        let filter = EventFilter {
            target_id: Some(entity.id),
            kinds: vec![EventKind::EntityCreated],
            verbs: vec!["create".into()],
            substrates: vec![SubstrateKind::Entity],
            after: Some(entity.created_at.saturating_sub(1)),
            ..EventFilter::default()
        };
        let page = PageRequest {
            offset: 0,
            limit: 1,
        };
        if !events
            .query_events(filter.clone(), page.clone())
            .await?
            .items
            .is_empty()
        {
            return Ok(());
        }

        let mut event = Event::new(
            entity.namespace.clone(),
            "create",
            EventKind::EntityCreated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(entity.id)
        .with_payload(serde_json::json!({
            "id": entity.id,
            "namespace": &entity.namespace,
            "kind": &entity.kind,
        }));
        let event_seed = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"khive:claimed-entity-create:v1");
        let mut event_key = Vec::with_capacity(24);
        event_key.extend_from_slice(entity.id.as_bytes());
        event_key.extend_from_slice(&entity.created_at.to_be_bytes());
        event.id = Uuid::new_v5(&event_seed, &event_key);
        if let Err(error) = events.append_event(event).await {
            if events.query_events(filter, page).await?.items.is_empty() {
                return Err(RuntimeError::Internal(format!(
                    "entity {} persists but its create event failed: {error}",
                    entity.id
                )));
            }
        }
        Ok(())
    }

    /// Repair a claimed row after an earlier post-insert indexing failure.
    /// This path is strict: any failed index stage names the still-live id.
    pub async fn reindex_claimed_entity(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<()> {
        if entity.namespace != token.namespace().as_str() || entity.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "entity {} is not a live row in the write namespace",
                entity.id
            )));
        }
        let doc = entity_fts_document(entity);
        let embed_body = doc.body.clone();
        #[cfg(any(test, feature = "fault-injection"))]
        let fts_inject = consume_fault(&FTS_FAIL_NS, &entity.namespace);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let fts_inject = false;
        let fts_result = if fts_inject {
            Err(RuntimeError::Internal("injected FTS failure".into()))
        } else {
            match self.text(token) {
                Ok(text) => text.upsert_document(doc).await.map_err(Into::into),
                Err(error) => Err(error),
            }
        };
        fts_result.map_err(|error| {
            RuntimeError::Internal(format!(
                "entity {} persists but its text index failed: {error}",
                entity.id
            ))
        })?;

        for model_name in self.registered_embedding_model_names() {
            let outcome = self
                .embed_document_with_model_outcome_for_token(token, &model_name, &embed_body)
                .await
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "entity {} persists but model {model_name} embedding failed: {error}",
                        entity.id
                    ))
                })?;
            #[cfg(any(test, feature = "fault-injection"))]
            let vector_inject = consume_fault(&VECTOR_FAIL_NS, &entity.namespace);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let vector_inject = false;
            if vector_inject {
                return Err(RuntimeError::Internal(format!(
                    "entity {} persists but model {model_name} vector indexing failed: injected vector failure",
                    entity.id
                )));
            }
            self.vectors_for_model(token, &model_name)
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "entity {} persists but model {model_name} vector store is unavailable: {error}",
                        entity.id
                    ))
                })?
                .insert(
                    entity.id,
                    SubstrateKind::Entity,
                    &entity.namespace,
                    "entity.body",
                    vec![outcome.vector],
                )
                .await
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "entity {} persists but model {model_name} vector indexing failed: {error}",
                        entity.id
                    ))
                })?;
        }
        Ok(())
    }

    /// Create and persist a new entity.
    ///
    /// Indexing failures trigger compensation across the entity row, FTS
    /// document, and any vector models touched by this call. If compensation
    /// also fails, the returned structured internal error identifies possible
    /// partial persistence, includes both failure classes, and carries the
    /// entity ID as a reconciliation handle.
    // REASON: entity creation requires kind, type, name, description, properties, tags, and
    // namespace token — refactoring into a builder would add indirection without reducing
    // caller complexity; this signature mirrors the MCP verb surface directly.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(crate) async fn create_entity(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<Entity> {
        let (entity, _, degradations) = self
            .create_entity_with_embedding_report_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                Vec::new(),
            )
            .await?;
        legacy_post_commit_result("create_entity", entity.id, entity, degradations)
    }

    /// Create an entity with role-keyed bytes already published to `BlobStore`.
    ///
    /// Every [`NewAttachment`] carries a typed content reference, so malformed
    /// references cannot enter through this consumer seam. Blob existence is
    /// checked before the database write. The entity row and all attachment rows
    /// then commit in one storage transaction; the FTS/vector compensation path
    /// hard-deletes the entity and its attachments together if a later indexing
    /// step fails. Published bytes remain recoverable by the BlobStore grace-period
    /// orphan policy when any post-publication step fails.
    /// A bounded embedding returns a non-retryable error carrying the committed
    /// entity ID and truncation report; use the report-aware variant to receive
    /// the entity and report together.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_attachments(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<Entity> {
        let (entity, embedding, degradations) = self
            .create_entity_with_attachments_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                attachments,
            )
            .await?;
        legacy_post_commit_result_with_embedding(
            "create_entity_with_attachments",
            entity.id,
            entity,
            embedding,
            degradations,
        )
    }

    /// Create an entity with attachments and retain embedding truncation accounting.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_attachments_and_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        let (entity, embedding, degradations) = self
            .create_entity_with_attachments_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                attachments,
            )
            .await?;
        legacy_post_commit_result(
            "create_entity_with_attachments_and_report",
            entity.id,
            (entity, embedding),
            degradations,
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_entity_with_attachments_inner(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<(
        Entity,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        // Attachment rows are the process-wide BlobStore's liveness authority.
        // Validate placement before existence probes or any record write: pack
        // runtimes bound to a secondary backend must explicitly call `core()`.
        drop(self.attachments()?);
        let blob_store = self.blob_store().ok_or_else(|| {
            RuntimeError::Unconfigured(
                "create_entity_with_attachments requires an installed BlobStore".to_string(),
            )
        })?;
        let mut roles = std::collections::HashSet::with_capacity(attachments.len());
        for attachment in &attachments {
            attachment.validate()?;
            if !roles.insert(attachment.role.as_str()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "duplicate attachment role {:?}",
                    attachment.role
                )));
            }
        }
        for attachment in &attachments {
            if !blob_store.exists(&attachment.content_ref).await? {
                return Err(RuntimeError::InvalidInput(format!(
                    "create_entity_with_attachments requires a published blob; no object exists for {}",
                    attachment.content_ref
                )));
            }
        }
        let validated_type = self.validate_entity_type_for_kind(kind, entity_type)?;
        let (entity, embedding, degradations) = self
            .create_entity_with_embedding_report_inner(
                token,
                kind,
                validated_type.as_deref(),
                name,
                description,
                properties,
                tags,
                attachments,
            )
            .await?;
        Ok((entity, embedding, degradations))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_embedding_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        let (entity, embedding, degradations) = self
            .create_entity_with_embedding_report_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                Vec::new(),
            )
            .await?;
        legacy_post_commit_result(
            "create_entity_with_embedding_report",
            entity.id,
            (entity, embedding),
            degradations,
        )
    }

    /// The committed entity and its non-retryable post-commit diagnostics.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<(
        Entity,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        self.create_entity_with_embedding_report_inner(
            token,
            kind,
            entity_type,
            name,
            description,
            properties,
            tags,
            Vec::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_entity_with_embedding_report_inner(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<(
        Entity,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        self.validate_entity_kind(kind)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        // Secret gate: scan name, description, structured properties, and tags.
        crate::secret_gate::check_at(name, "entity", "name")?;
        if let Some(d) = description {
            crate::secret_gate::check_at(d, "entity", "description")?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json_at(p, "entity", "properties")?;
        }
        crate::secret_gate::check_tags_at(&tags, "entity", "tags")?;
        let ns = token.namespace().as_str();
        let mut entity = Entity::new(ns, kind, name).with_entity_type(entity_type);
        if let Some(d) = description {
            entity = entity.with_description(d);
        }
        if let Some(p) = properties {
            entity = entity.with_properties(p);
        }
        if !tags.is_empty() {
            entity = entity.with_tags(tags);
        }
        let projected_content_ref = attachments
            .iter()
            .find(|attachment| attachment.role == "content")
            .map(|attachment| attachment.content_ref.to_string());
        let attachment_rows = attachments
            .into_iter()
            .map(|attachment| {
                Attachment::from_new(
                    entity.id,
                    AttachmentSubstrate::Entity,
                    attachment,
                    entity.created_at,
                )
            })
            .collect();
        self.entities(token)?
            .upsert_entity_with_attachments(entity.clone(), attachment_rows)
            .await?;
        entity.content_ref = projected_content_ref;

        let doc = entity_fts_document(&entity);
        let embed_body = doc.body.clone();

        // FTS step — compensate entity row on failure (mirrors create_note_inner).
        {
            #[cfg(any(test, feature = "fault-injection"))]
            let fts_inject = consume_fault(&FTS_FAIL_NS, ns);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let fts_inject = false;
            let fts_result: RuntimeResult<()> = if fts_inject {
                Err(RuntimeError::Internal("injected FTS failure".to_string()))
            } else {
                match self.text(token) {
                    Ok(fts) => fts.upsert_document(doc).await.map_err(RuntimeError::from),
                    Err(e) => Err(e),
                }
            };
            if let Err(e) = fts_result {
                let cleanup_errors = self
                    .compensate_entity_create(token, entity.id, ns, &[])
                    .await;
                return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
            }
        }

        // Vector embedding + insert step — compensate entity row + FTS doc on failure.
        // Fan out to ALL registered models (mirrors create_note_inner multi-model path).
        let embed_model_names = {
            let names = self.registered_embedding_model_names();
            if names.is_empty() {
                vec![]
            } else {
                names
            }
        };

        let mut embedding_report = crate::retrieval::EmbeddingTruncationReport::default();
        if embed_model_names.len() == 1 {
            let model_name = &embed_model_names[0];
            let vec_result = self
                .embed_document_with_model_outcome_for_token(token, model_name, &embed_body)
                .await;

            #[cfg(any(test, feature = "fault-injection"))]
            let vec_inject = consume_fault(&VECTOR_FAIL_NS, ns);
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

            let single_result: RuntimeResult<()> = match vec_result {
                Ok(outcome) => {
                    embedding_report.observe(&outcome);
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => vs
                            .insert(
                                entity.id,
                                SubstrateKind::Entity,
                                ns,
                                "entity.body",
                                vec![outcome.vector],
                            )
                            .await
                            .map_err(RuntimeError::from),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(e) = single_result {
                let cleanup_errors = self
                    .compensate_entity_create(
                        token,
                        entity.id,
                        ns,
                        std::slice::from_ref(model_name),
                    )
                    .await;
                return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
            }
        } else if !embed_model_names.is_empty() {
            // Multi-model path: embed with each model in parallel, then insert sequentially
            // with inserted_models tracking for rollback on partial failure.
            let rt_clone = self.clone();
            let body_owned = embed_body.clone();
            let usage_ctx = crate::usage::current();
            let mut join_set = tokio::task::JoinSet::new();
            for (idx, model_name) in embed_model_names.iter().enumerate() {
                let rt = rt_clone.clone();
                let text = body_owned.clone();
                let name = model_name.clone();
                let ctx = usage_ctx.clone();
                let token = (*token).clone();
                join_set.spawn(crate::runtime::inherit_request_embedder_scope(async move {
                    let fut = rt.embed_document_with_model_outcome_for_token(&token, &name, &text);
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
                    let cleanup_errors = self
                        .compensate_entity_create(token, entity.id, ns, &[])
                        .await;
                    return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
                }
            };
            // TODO(P2): parallelize vector inserts
            let mut inserted_models: Vec<String> = Vec::with_capacity(embed_model_names.len());
            for (model_name, outcome) in embed_model_names.iter().zip(outcomes) {
                embedding_report.observe(&outcome);
                // Count-targetable fault injection for multi-model insert path.
                #[cfg(any(test, feature = "fault-injection"))]
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
                #[cfg(not(any(test, feature = "fault-injection")))]
                let count_inject = false;

                let insert_result = if count_inject {
                    Err(RuntimeError::Internal(
                        "injected vector insert failure".to_string(),
                    ))
                } else {
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => vs
                            .insert(
                                entity.id,
                                SubstrateKind::Entity,
                                ns,
                                "entity.body",
                                vec![outcome.vector],
                            )
                            .await
                            .map_err(RuntimeError::from),
                        Err(e) => Err(e),
                    }
                };
                if let Err(e) = insert_result {
                    // Include the model whose INSERT returned an error: a backend
                    // error does not prove the write had no side effects.
                    let mut cleanup_models = inserted_models.clone();
                    cleanup_models.push(model_name.clone());
                    let cleanup_errors = self
                        .compensate_entity_create(token, entity.id, ns, &cleanup_models)
                        .await;
                    return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
                }
                inserted_models.push(model_name.clone());
            }
        }

        // The arrival event, appended only after every compensating step has had
        // its chance to fire: a create that rolled back returns above and never
        // reaches here, so the event plane cannot name an entity that does not
        // exist. Deletes and updates already emitted theirs; creates did not,
        // which left the audit trail able to say what left the graph and not
        // what entered it.
        let created_event = khive_storage::event::Event::new(
            entity.namespace.clone(),
            "create",
            EventKind::EntityCreated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(entity.id)
        .with_payload(serde_json::json!({
            "id": entity.id,
            "namespace": entity.namespace,
            "kind": entity.kind,
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
                "create_entity",
                entity.id,
                "event_append",
                error,
            );
        }

        Ok((entity, embedding_report, degradations))
    }

    /// Retrieve an entity by ID.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    ///
    /// Interim identifier-continuity disclosure (precedes the full transitive
    /// redirect chase): a miss is probed once against the tombstone row. If
    /// the id was consumed by `merge(into_id, from_id)` — `merged_into` set —
    /// the `NotFound` message names the kept id so the caller can requery it
    /// directly. Single-level only: it does not chase a chain of merges and
    /// does not return the kept entity in place of the miss. The probe only
    /// runs after the live-row lookup misses, so the happy path pays no
    /// extra query.
    pub async fn get_entity(&self, token: &NamespaceToken, id: Uuid) -> RuntimeResult<Entity> {
        let store = self.entities(token)?;
        if let Some(entity) = store.get_entity(id).await? {
            return Ok(entity);
        }
        if let Some(tombstone) = store.get_entity_including_deleted(id).await? {
            if let Some(kept_id) = tombstone.merged_into {
                return Err(RuntimeError::NotFound(format!(
                    "{id} was merged into {kept_id}; query the kept id"
                )));
            }
        }
        Err(RuntimeError::NotFound(format!("entity {id}")))
    }

    /// Retrieve an entity by ID including soft-deleted rows.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn get_entity_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Entity>> {
        self.entities(token)?
            .get_entity_including_deleted(id)
            .await
            .map_err(Into::into)
    }

    /// Retrieve a note by ID including soft-deleted rows.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn get_note_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<khive_storage::note::Note>> {
        self.notes(token)?
            .get_note_including_deleted(id)
            .await
            .map_err(Into::into)
    }

    /// Fetch multiple entities by ID, returning only those that exist in the
    /// caller's namespace.  Missing or namespace-mismatched IDs are silently
    /// omitted so that batch lookups don't abort on a single stale reference.
    pub async fn get_entities_by_ids(
        &self,
        token: &NamespaceToken,
        ids: &[Uuid],
    ) -> RuntimeResult<Vec<Entity>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let filter = EntityFilter {
            ids: ids.to_vec(),
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: 0,
                    limit: ids.len() as u32,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Like `get_entities_by_ids` but scoped to the token's full visible-namespace
    /// set (`primary ∪ extra_visible`) instead of primary only.
    ///
    /// Graph expansion (`neighbors`, `traverse`) iterates over all visible
    /// namespaces, so enrichment must use the same scope — otherwise neighbors
    /// or path nodes whose entities live in an extra-visible namespace are left
    /// with `name = None`, `kind = None`.  Missing or out-of-scope IDs are
    /// silently omitted (best-effort, same as `get_entities_by_ids`).
    async fn get_entities_by_ids_visible(
        &self,
        token: &NamespaceToken,
        ids: &[Uuid],
    ) -> RuntimeResult<Vec<Entity>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            ids: ids.to_vec(),
            namespaces,
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: 0,
                    limit: ids.len() as u32,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Enforce that `record_ns` is within the caller's visible namespace set.
    ///
    /// Returns `Err(NotFound)` when the record namespace is not in the visible
    /// set — wrong-namespace and absent UUIDs must be indistinguishable
    /// externally (no existence oracle).
    ///
    /// When the visible set is a single entry equal to `caller_primary_ns`, this
    /// is identical to the former strict-equality check (backward-compatible).
    pub(crate) fn ensure_namespace(record_ns: &str, caller_primary_ns: &str) -> RuntimeResult<()> {
        if record_ns == caller_primary_ns {
            return Ok(());
        }
        Err(RuntimeError::NotFound("not found in this namespace".into()))
    }

    /// Enforce that `record_ns` is a member of the token's visible namespace set.
    ///
    /// This is the multi-namespace-aware variant used when the token carries an
    /// extended visibility set. For single-namespace tokens (visible == [primary])
    /// this degenerates to the same strict-equality check as `ensure_namespace`.
    pub(crate) fn ensure_namespace_visible(
        record_ns: &str,
        token: &NamespaceToken,
    ) -> RuntimeResult<()> {
        for ns in token.visible_namespaces() {
            if record_ns == ns.as_str() {
                return Ok(());
            }
        }
        Err(RuntimeError::NotFound("not found in this namespace".into()))
    }

    /// List entities visible to the token, optionally filtered by kind and entity_type.
    /// A null entity_type falls back to a string properties.type for filtering only.
    ///
    /// When the token carries a multi-namespace visible set, entities from all
    /// visible namespaces are returned. When the visible set is `[primary]`
    /// (the default) this behaves identically to the pre-visibility behaviour.
    pub async fn list_entities(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        entity_type: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let filter = EntityFilter {
            kinds: kind
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            entity_types: entity_type
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            legacy_entity_type_fallback: true,
            ..Default::default()
        };
        self.list_entities_filtered(token, filter, limit, offset)
            .await
    }

    /// Apply a composed entity predicate before offset pagination. Namespace
    /// visibility is supplied by the token, just as for the scalar list API.
    pub async fn list_entities_filtered(
        &self,
        token: &NamespaceToken,
        mut filter: EntityFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        filter.namespaces = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let page = self
            .entities(token)?
            .query_entities_count_free(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// List an immutable insertion-sequence page of visible entities.
    ///
    /// The public cursor remains the UUID of the last returned entity. We
    /// resolve its immutable database-assigned sequence before querying so callers do
    /// not need to serialize storage details. A missing or out-of-scope cursor
    /// fails explicitly instead of silently resuming from the wrong boundary.
    pub async fn list_entities_after(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Entity>, Option<Uuid>)> {
        let filter = EntityFilter {
            kinds: kind
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            entity_types: entity_type
                .map(|value| vec![value.to_string()])
                .unwrap_or_default(),
            legacy_entity_type_fallback: true,
            tags_any: tags_any.to_vec(),
            ..Default::default()
        };
        self.list_entities_after_filtered(token, filter, after, limit)
            .await
    }

    /// Apply a composed entity predicate before insertion-sequence pagination,
    /// preserving the scalar API's cursor validation and token visibility.
    pub async fn list_entities_after_filtered(
        &self,
        token: &NamespaceToken,
        mut filter: EntityFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Entity>, Option<Uuid>)> {
        let store = self.entities(token)?;
        let after = match after {
            Some(id) => {
                let entity = self
                    .get_entity_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("entity cursor {id}")))?;
                Self::ensure_namespace_visible(&entity.namespace, token)?;
                let sequence = store.entity_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "entity cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
            None => None,
        };
        filter.namespaces = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let page = store
            .query_entities_after(token.namespace().as_str(), filter, after, limit)
            .await?;
        Ok((page.items, page.next_after.map(|cursor| cursor.id)))
    }

    /// List entities filtered by kind, optional domain tag, limit, and offset.
    ///
    /// When `domain_tag` is Some, the query is restricted at the storage layer via
    /// `EntityFilter::tags_any` so the page result already reflects the domain
    /// constraint.  This avoids the silent truncation that occurs when filtering
    /// post-page (K-3). Multi-namespace visibility from the token is applied.
    pub async fn list_entities_tagged(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        domain_tag: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Entity>> {
        let ns_strs: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            tags_any: match domain_tag {
                Some(t) if !t.is_empty() => vec![t.to_string()],
                _ => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities_count_free(
                token.namespace().as_str(),
                filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Count entities filtered by kind and optional domain tag.
    ///
    /// Used to report a meaningful `total` alongside a paginated listing (K-6).
    /// Multi-namespace visibility from the token is applied.
    pub async fn count_entities_tagged(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        domain_tag: Option<&str>,
    ) -> RuntimeResult<u64> {
        let ns_strs: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            tags_any: match domain_tag {
                Some(t) if !t.is_empty() => vec![t.to_string()],
                _ => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        Ok(self
            .entities(token)?
            .count_entities(token.namespace().as_str(), filter)
            .await?)
    }

    /// List events in the namespace proven by the caller token.
    pub async fn list_events(
        &self,
        token: &NamespaceToken,
        filter: EventFilter,
        page: PageRequest,
    ) -> RuntimeResult<Page<Event>> {
        self.events(token)?
            .query_events(filter, page)
            .await
            .map_err(Into::into)
    }

    // ---- Edge operations ----

    /// Validate that `source_id` and `target_id` are legal endpoints for `relation`.
    ///
    /// Centralises the three-case relation contract so that both
    /// `link()` and `update_edge()` share identical enforcement:
    ///
    /// - `annotates`: source MUST be a note; target may be any substrate.
    /// - `supersedes` / `supports` / `refutes`: same-substrate only (note→note or entity→entity).
    /// - All other relations: both endpoints MUST be entities.
    ///
    /// Returns the validated endpoint substrates when valid; otherwise
    /// `InvalidInput` or `NotFound` for an invalid endpoint pair.
    ///
    /// `pub(crate)`: the atomic prepare pass (`crate::atomic_prepare`) reuses
    /// this exact endpoint-type validation during its async prepare step,
    /// before building a `LinkPlan`, rather than re-deriving the checks.
    pub(crate) async fn validate_edge_relation_endpoints(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<(EdgeEndpointKind, EdgeEndpointKind)> {
        if source_id == target_id {
            return Err(RuntimeError::InvalidInput(
                "self-loop edges are not allowed: source_id and target_id must be different".into(),
            ));
        }
        if relation == EdgeRelation::Annotates {
            // Source must be a note. By-ID endpoint resolution is namespace-agnostic:
            // link consumes two by-ID endpoints, so it must resolve exactly what
            // get() resolves, regardless of caller namespace.
            match self.resolve_edge_endpoint(token, source_id).await? {
                Some(Resolved::Note(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "annotates source {source_id} must be a note"
                    )));
                }
                None => {
                    // Existing edge used as annotates source: wrong kind, not absent.
                    if self.get_edge(token, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "annotates source {source_id} must be a note"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            }
            // Target may be any substrate (entity, note, event, or edge) — by-ID, unfiltered.
            let target_kind = match self.resolve_edge_endpoint(token, target_id).await? {
                Some(Resolved::Entity(_)) => EdgeEndpointKind::Entity,
                Some(Resolved::Note(_)) => EdgeEndpointKind::Note,
                Some(Resolved::Event(_)) => EdgeEndpointKind::Event,
                Some(Resolved::PackRecord { .. }) => {
                    return Err(RuntimeError::InvalidInput(
                        "pack-private record is not a valid edge endpoint for annotates".into(),
                    ));
                }
                None => match self.get_edge(token, target_id).await {
                    Ok(Some(_)) => EdgeEndpointKind::Edge,
                    Ok(None) | Err(RuntimeError::NotFound(_)) => {
                        return Err(RuntimeError::NotFound(format!(
                            "link target {target_id} not found"
                        )));
                    }
                    Err(error) => return Err(error),
                },
            };
            return Ok((EdgeEndpointKind::Note, target_kind));
        } else if crate::pack::is_special_relation(relation) {
            // supersedes / supports / refutes: same-substrate only (note→note or entity→entity).
            // Event and edge endpoints are invalid regardless of the other endpoint.
            // Endpoint resolution is by-ID and namespace-agnostic.
            let rel_name = relation.as_str();
            let src = match self.resolve_edge_endpoint(token, source_id).await? {
                Some(r) => r,
                None => {
                    if self.get_edge(token, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "{rel_name} source {source_id} must be a note or entity (got edge)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            };
            let tgt = match self.resolve_edge_endpoint(token, target_id).await? {
                Some(r) => r,
                None => {
                    if self.get_edge(token, target_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "{rel_name} target {target_id} must be a note or entity (got edge)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link target {target_id} not found"
                    )));
                }
            };
            return match (&src, &tgt) {
                (Resolved::Entity(src_e), Resolved::Entity(tgt_e)) => {
                    if !base_entity_rule_allows(&src_e.kind, relation, &tgt_e.kind) {
                        let legal_relations = accepted_entity_relations_description(
                            &self.pack_edge_rules(),
                            &src_e.kind,
                            src_e.entity_type.as_deref(),
                            &tgt_e.kind,
                            tgt_e.entity_type.as_deref(),
                        );
                        let rule_hint = match relation {
                            EdgeRelation::Supports | EdgeRelation::Refutes => {
                                "requires concept|document|dataset|artifact -> concept \
                                 (or same-substrate note -> note)"
                            }
                            _ => "requires same-kind entity endpoints",
                        };
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[{rel_name}]-> ({}) is not in the base endpoint \
                             allowlist; {rel_name} {rule_hint}; currently legal relations for \
                             {} -> {} under the loaded endpoint rules: {legal_relations}",
                            src_e.kind, tgt_e.kind, src_e.kind, tgt_e.kind
                        )));
                    }
                    Ok((EdgeEndpointKind::Entity, EdgeEndpointKind::Entity))
                }
                (Resolved::Note(_), Resolved::Note(_)) => {
                    Ok((EdgeEndpointKind::Note, EdgeEndpointKind::Note))
                }
                (Resolved::Event(_), _) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} does not apply to events; source {source_id} is an event"
                    )));
                }
                (_, Resolved::Event(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} does not apply to events; target {target_id} is an event"
                    )));
                }
                (Resolved::Entity(_), Resolved::Note(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate (note→note or entity→entity); \
                         got source={source_id} (entity) target={target_id} (note)"
                    )));
                }
                (Resolved::Note(_), Resolved::Entity(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate (note→note or entity→entity); \
                         got source={source_id} (note) target={target_id} (entity)"
                    )));
                }
                (Resolved::PackRecord { .. }, _) | (_, Resolved::PackRecord { .. }) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "pack-private record is not a valid edge endpoint for {rel_name}"
                    )));
                }
            };
        } else {
            // All remaining base relations require entity→entity with kind-level
            // restrictions (see base allowlist). Packs may extend the allowlist
            // additively via EDGE_RULES.
            //
            // Strategy: resolve both endpoints once (by-ID, unfiltered), consult pack
            // rules; on miss, fall through to the original base-rule error messages.
            let src_res = self.resolve_edge_endpoint(token, source_id).await?;
            let tgt_res = self.resolve_edge_endpoint(token, target_id).await?;
            let pack_rules = self.pack_edge_rules();

            if pack_rule_allows(&pack_rules, relation, src_res.as_ref(), tgt_res.as_ref()) {
                let kind = |resolved: Option<&Resolved>| match resolved {
                    Some(Resolved::Entity(_)) => Some(EdgeEndpointKind::Entity),
                    Some(Resolved::Note(_)) => Some(EdgeEndpointKind::Note),
                    _ => None,
                };
                return match (kind(src_res.as_ref()), kind(tgt_res.as_ref())) {
                    (Some(source_kind), Some(target_kind)) => Ok((source_kind, target_kind)),
                    _ => Err(RuntimeError::Internal(
                        "pack endpoint rule admitted an unsupported substrate".into(),
                    )),
                };
            }

            // Substrate check: both endpoints must be entities.
            let (src_kind, src_entity_type) = match src_res.as_ref() {
                Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "link source {source_id} must be an entity for relation {relation:?} \
                         (only `annotates` crosses substrates)"
                    )));
                }
                None => {
                    if self.get_edge(token, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "link source {source_id} must be an entity for relation {relation:?} \
                             (only `annotates` crosses substrates)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            };
            let (tgt_kind, tgt_entity_type) = match tgt_res.as_ref() {
                Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "link target {target_id} must be an entity for relation {relation:?} \
                         (only `annotates` crosses substrates)"
                    )));
                }
                None => {
                    if self.get_edge(token, target_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "link target {target_id} must be an entity for relation {relation:?} \
                             (only `annotates` crosses substrates)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link target {target_id} not found"
                    )));
                }
            };
            if !base_entity_rule_allows(src_kind, relation, tgt_kind) {
                let legal_relations = accepted_entity_relations_description(
                    &pack_rules,
                    src_kind,
                    src_entity_type,
                    tgt_kind,
                    tgt_entity_type,
                );
                return Err(RuntimeError::InvalidInput(format!(
                    "({src_kind}) -[{}]-> ({tgt_kind}) is not in the base endpoint \
                     allowlist; use pack EDGE_RULES to extend the allowlist; currently legal \
                     relations for {src_kind} -> {tgt_kind} under the loaded endpoint rules: \
                     {legal_relations}",
                    relation.as_str()
                )));
            }
        }
        Ok((EdgeEndpointKind::Entity, EdgeEndpointKind::Entity))
    }

    /// Public delegator for cross-backend link validation.
    ///
    /// Exposes `validate_edge_relation_endpoints` for the `SubstrateCoordinator`
    /// so it can validate the relation before writing the edge on the source backend.
    pub async fn validate_link_endpoints(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<()> {
        self.validate_edge_relation_endpoints(token, source_id, target_id, relation)
            .await
            .map(|_| ())
    }

    /// Validate an edge relation using pre-fetched endpoint records.
    ///
    /// For cross-backend links the source and target live on different backends —
    /// the source runtime cannot resolve the target. The coordinator fetches each
    /// endpoint from its own backend, then calls this method to enforce the
    /// kind-pairing rules without a second DB round-trip.
    ///
    /// `src` and `tgt` are the `resolve_edge_endpoint` results from each backend. The
    /// `token` supplies the pack edge rules installed on this (source) runtime;
    /// no DB access is performed.
    pub fn validate_link_endpoints_by_resolved(
        &self,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        src: Option<&Resolved>,
        tgt: Option<&Resolved>,
    ) -> RuntimeResult<()> {
        if source_id == target_id {
            return Err(RuntimeError::InvalidInput(
                "self-loop edges are not allowed: source_id and target_id must be different".into(),
            ));
        }

        if relation == EdgeRelation::Annotates {
            match src {
                Some(Resolved::Note(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "annotates source {source_id} must be a note"
                    )));
                }
                None => {
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            }
            if tgt.is_none() {
                return Err(RuntimeError::NotFound(format!(
                    "link target {target_id} not found"
                )));
            }
            return Ok(());
        }

        if crate::pack::is_special_relation(relation) {
            let rel_name = relation.as_str();
            let src = src.ok_or_else(|| {
                RuntimeError::NotFound(format!("link source {source_id} not found"))
            })?;
            let tgt = tgt.ok_or_else(|| {
                RuntimeError::NotFound(format!("link target {target_id} not found"))
            })?;
            match (src, tgt) {
                (Resolved::Entity(src_e), Resolved::Entity(tgt_e)) => {
                    if !base_entity_rule_allows(&src_e.kind, relation, &tgt_e.kind) {
                        let legal_relations = accepted_entity_relations_description(
                            &self.pack_edge_rules(),
                            &src_e.kind,
                            src_e.entity_type.as_deref(),
                            &tgt_e.kind,
                            tgt_e.entity_type.as_deref(),
                        );
                        let rule_hint = match relation {
                            EdgeRelation::Supports | EdgeRelation::Refutes => {
                                "requires concept|document|dataset|artifact -> concept \
                                 (or same-substrate note -> note)"
                            }
                            _ => "requires same-kind entity endpoints",
                        };
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[{rel_name}]-> ({}) is not in the base endpoint \
                             allowlist; {rel_name} {rule_hint}; currently legal relations for \
                             {} -> {} under the loaded endpoint rules: {legal_relations}",
                            src_e.kind, tgt_e.kind, src_e.kind, tgt_e.kind
                        )));
                    }
                }
                (Resolved::Note(_), Resolved::Note(_)) => {}
                (Resolved::Entity(_), Resolved::Note(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate \
                         (note→note or entity→entity); got source={source_id} (entity) \
                         target={target_id} (note)"
                    )));
                }
                (Resolved::Note(_), Resolved::Entity(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate \
                         (note→note or entity→entity); got source={source_id} (note) \
                         target={target_id} (entity)"
                    )));
                }
                (Resolved::PackRecord { .. }, _) | (_, Resolved::PackRecord { .. }) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "pack-private record is not a valid edge endpoint for {rel_name}"
                    )));
                }
                _ => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be notes or entities (not events)"
                    )));
                }
            }
            return Ok(());
        }

        // All remaining base relations: entity→entity with kind-level restrictions.
        // Consult pack rules installed on this (source) runtime first.
        let pack_rules = self.pack_edge_rules();
        if pack_rule_allows(&pack_rules, relation, src, tgt) {
            return Ok(());
        }

        let (src_kind, src_entity_type) = match src {
            Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "link source {source_id} must be an entity for relation {relation:?} \
                     (only `annotates` crosses substrates)"
                )));
            }
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "link source {source_id} not found"
                )));
            }
        };
        let (tgt_kind, tgt_entity_type) = match tgt {
            Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "link target {target_id} must be an entity for relation {relation:?} \
                     (only `annotates` crosses substrates)"
                )));
            }
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "link target {target_id} not found"
                )));
            }
        };

        if !base_entity_rule_allows(src_kind, relation, tgt_kind) {
            let legal_relations = accepted_entity_relations_description(
                &pack_rules,
                src_kind,
                src_entity_type,
                tgt_kind,
                tgt_entity_type,
            );
            return Err(RuntimeError::InvalidInput(format!(
                "({src_kind}) -[{}]-> ({tgt_kind}) is not in the base endpoint \
                 allowlist; use pack EDGE_RULES to extend the allowlist; currently legal relations \
                 for {src_kind} -> {tgt_kind} under the loaded endpoint rules: {legal_relations}",
                relation.as_str()
            )));
        }

        Ok(())
    }

    /// Validate an `annotates` edge relation using pre-located endpoint kinds.
    ///
    /// Sibling of [`Self::validate_link_endpoints_by_resolved`] for callers that
    /// only have an [`EdgeEndpointKind`] (entity/note/event/edge) rather than a
    /// full [`Resolved`] record — the `SubstrateCoordinator`'s cross-backend
    /// `locate_endpoint` resolves edge-substrate UUIDs too (matching `get`'s
    /// by-ID resolution order), but edges have no `Resolved` variant, so
    /// `validate_link_endpoints_by_resolved` cannot express them.
    ///
    /// `annotates` is the only relation this covers: source must be a note,
    /// target may be any substrate (entity, note, event, or edge).
    pub fn validate_annotates_endpoint_kinds(
        &self,
        source_id: Uuid,
        target_id: Uuid,
        source: Option<EdgeEndpointKind>,
        target: Option<EdgeEndpointKind>,
    ) -> RuntimeResult<()> {
        if source_id == target_id {
            return Err(RuntimeError::InvalidInput(
                "self-loop edges are not allowed: source_id and target_id must be different".into(),
            ));
        }
        match source {
            Some(EdgeEndpointKind::Note) => {}
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "annotates source {source_id} must be a note"
                )));
            }
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "link source {source_id} not found"
                )));
            }
        }
        if target.is_none() {
            return Err(RuntimeError::NotFound(format!(
                "link target {target_id} not found"
            )));
        }
        Ok(())
    }

    /// Create a directed edge between two substrates.
    ///
    /// Enforces the three-case relation contract via
    /// `validate_edge_relation_endpoints`. See that method for the full contract.
    ///
    /// For symmetric relations (`competes_with`, `composed_with`) the endpoint
    /// pair is canonicalised to `source_uuid < target_uuid` so that A→B and B→A
    /// deduplicate to one row.
    ///
    /// `metadata` is validated against governed keys; `dependency_kind` is
    /// inferred for `depends_on` edges when absent.
    ///
    /// `target_backend` is always `None` for locally-routed edges written through
    /// this path. Both endpoints must exist in the local namespace, so setting
    /// `target_backend = None` is the only valid choice.
    ///
    /// Endpoint existence is a by-ID check and namespace-agnostic: a record
    /// that exists in a different namespace than the caller still resolves,
    /// exactly as `get()` would.
    pub async fn link(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
    ) -> RuntimeResult<Edge> {
        self.link_observed(
            token, source_id, target_id, relation, weight, metadata, false,
        )
        .await
        .map(|result| result.edge)
    }

    /// Observable form of [`Self::link`]. Live natural-key conflicts retain
    /// the accepted replace semantics, while tombstones require the explicit
    /// `resurrect` opt-in. The returned preimage and disposition are derived
    /// inside the graph writer transaction and drive the lifecycle event.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_observed(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        resurrect: bool,
    ) -> RuntimeResult<EdgeUpsertResult> {
        validate_edge_weight(weight)?;
        validate_edge_metadata(relation, metadata.as_ref())?;
        let (source_kind, target_kind) = self
            .validate_edge_relation_endpoints(token, source_id, target_id, relation)
            .await?;
        let (canonical_source, canonical_target) =
            canonical_edge_endpoints(relation, source_id, target_id);
        let (source_kind, target_kind) =
            canonical_edge_endpoint_kinds(source_id, canonical_source, source_kind, target_kind);
        let (source_id, target_id) = (canonical_source, canonical_target);
        let metadata = if relation == EdgeRelation::DependsOn {
            // By-ID, unfiltered — matches the namespace-agnostic endpoint validation
            // above. The visible-set-scoped `resolve` would silently drop the
            // dependency_kind inference for endpoints validation now allows outside
            // the caller's visible set.
            match (
                self.resolve_edge_endpoint(token, source_id).await?,
                self.resolve_edge_endpoint(token, target_id).await?,
            ) {
                (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                    merge_dependency_kind(&src_e.kind, &tgt_e.kind, metadata)
                }
                _ => metadata,
            }
        } else {
            metadata
        };
        validate_edge_metadata(relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        let ns = token.namespace().as_str();
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: ns.to_string(),
            source_id,
            target_id,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata,
            target_backend: None,
        };
        // `upsert_edge_guarded` re-checks both endpoints exist as part of the same
        // write, not the separate `validate_edge_relation_endpoints` read above: a
        // concurrent hard-delete landing between that read and this write can no
        // longer create a durably dangling edge. Which endpoint(s) were missing is
        // reported by the guard's own in-transaction probe (`GuardedWriteOutcome::
        // Refused`), not reconstructed here by re-reading the endpoints after the
        // fact: a second concurrent write landing between the refusal and a
        // post-hoc read could otherwise misreport which endpoint was actually
        // missing at write time.
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::Single {
                request: EdgeUpsertRequest { edge, resurrect },
                guard_endpoints: true,
            },
            GraphMutationPreconditions::default(),
            Vec::new(),
            move |outcome| match &outcome.mutation {
                GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(result)) => {
                    Ok(vec![Self::link_mutation_event(
                        &attribution,
                        result,
                        source_kind,
                        target_kind,
                    )])
                }
                _ => Err(Self::link_composition_shape_error(
                    "expected a written singleton",
                )),
            },
        )
        .await?;
        let GraphMutationOutcome::Single(outcome) = outcome.mutation else {
            return Err(RuntimeError::Internal(
                "link: unexpected composition outcome".into(),
            ));
        };
        let result = match outcome {
            GuardedEdgeUpsertOutcome::Written(result) => result,
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::MissingEndpoints(missing)) => {
                return Err(RuntimeError::GuardedWriteFailed(GuardedWriteFailure {
                    entry_index: None,
                    missing_source: missing.source.then_some(source_id),
                    missing_target: missing.target.then_some(target_id),
                }));
            }
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::ResurrectionRequired { edge }) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "edge {} is soft-deleted; pass resurrect=true to link explicitly",
                    edge.id
                )))
            }
        };
        Ok(result)
    }

    /// Write an edge with an explicit `target_backend` stamp (ADR-029 D3).
    ///
    /// Called by the `SubstrateCoordinator` when source and target are on
    /// different backends. The coordinator validates endpoints before calling
    /// this method via [`Self::validate_link_endpoints`], and supplies the
    /// resolved endpoint kinds for the lifecycle event. Endpoint validation is
    /// skipped here. The edge is written on the source backend only.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_with_target_backend(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        source_kind: EdgeEndpointKind,
        target_kind: EdgeEndpointKind,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        target_backend: Option<String>,
    ) -> RuntimeResult<Edge> {
        self.link_with_target_backend_observed(
            token,
            source_id,
            target_id,
            source_kind,
            target_kind,
            relation,
            weight,
            metadata,
            target_backend,
            false,
        )
        .await
        .map(|result| result.edge)
    }

    /// Policy-aware cross-backend form of [`Self::link_observed`]. Endpoint
    /// validation remains the coordinator's responsibility; mutation
    /// classification and tombstone handling stay inside the source store.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_with_target_backend_observed(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        source_kind: EdgeEndpointKind,
        target_kind: EdgeEndpointKind,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        target_backend: Option<String>,
        resurrect: bool,
    ) -> RuntimeResult<EdgeUpsertResult> {
        validate_edge_weight(weight)?;
        let (canonical_source, canonical_target) =
            canonical_edge_endpoints(relation, source_id, target_id);
        let (source_kind, target_kind) =
            canonical_edge_endpoint_kinds(source_id, canonical_source, source_kind, target_kind);
        let (source_id, target_id) = (canonical_source, canonical_target);
        validate_edge_metadata(relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        let ns = token.namespace().as_str();
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: ns.to_string(),
            source_id,
            target_id,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata,
            target_backend,
        };
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::Single {
                request: EdgeUpsertRequest { edge, resurrect },
                guard_endpoints: false,
            },
            GraphMutationPreconditions::default(),
            Vec::new(),
            move |outcome| match &outcome.mutation {
                GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(result)) => {
                    Ok(vec![Self::link_mutation_event(
                        &attribution,
                        result,
                        source_kind,
                        target_kind,
                    )])
                }
                _ => Err(Self::link_composition_shape_error(
                    "expected a written singleton",
                )),
            },
        )
        .await?;
        match outcome.mutation {
            GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(result)) => Ok(result),
            GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Refused(
                EdgeUpsertRefusal::ResurrectionRequired { edge },
            )) => {
                let error = khive_storage::StorageError::Conflict {
                    capability: khive_storage::StorageCapability::Graph,
                    operation: "upsert_edge_observed".into(),
                    message: format!(
                        "edge {} is soft-deleted; explicit resurrection is required",
                        edge.id,
                    ),
                };
                Err(RuntimeError::InvalidInput(format!(
                    "edge natural key is soft-deleted; pass resurrect=true to link explicitly: {error}"
                )))
            }
            _ => Err(RuntimeError::Internal(
                "link: unexpected composition outcome".into(),
            )),
        }
    }

    fn link_mutation_event(
        attribution: &crate::EventAttribution,
        result: &EdgeUpsertResult,
        source_kind: EdgeEndpointKind,
        target_kind: EdgeEndpointKind,
    ) -> Event {
        let kind = match result.disposition {
            EdgeUpsertDisposition::Created => EventKind::LinkCreated,
            EdgeUpsertDisposition::Updated | EdgeUpsertDisposition::Resurrected => {
                EventKind::EdgeUpdated
            }
        };
        let edge_id = Uuid::from(result.edge.id);
        let mut payload = serde_json::json!({
            "id": edge_id,
            "namespace": result.edge.namespace,
            "mutation": result.disposition.name(),
            "source_id": result.edge.source_id,
            "target_id": result.edge.target_id,
            "relation": result.edge.relation,
            "weight": result.edge.weight,
            "metadata": result.edge.metadata,
            "previous": result.previous,
        });
        if kind == EventKind::LinkCreated {
            payload["source_kind"] = serde_json::json!(source_kind.name());
            payload["target_kind"] = serde_json::json!(target_kind.name());
        }
        attribution.stamp(
            Event::new(
                result.edge.namespace.clone(),
                "link",
                kind,
                SubstrateKind::Entity,
                "",
            )
            .with_target(edge_id)
            .with_payload(payload),
        )
    }

    fn link_composition_shape_error(message: &'static str) -> khive_storage::StorageError {
        khive_storage::StorageError::InvalidInput {
            capability: khive_storage::StorageCapability::Graph,
            operation: "link_mutation_event".into(),
            message: message.into(),
        }
    }

    /// Returns `true` if `id` resolves to a live substrate record in the
    /// caller's visible namespace set.
    ///
    /// Covers entity, note, event (via `resolve`) and edge (via `get_edge_visible`).
    /// Only records that are accessible to the caller (primary or configured visible
    /// namespaces) return `true`; absent or foreign-invisible records return `false`.
    pub(crate) async fn substrate_exists_in_ns(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<bool> {
        if self.resolve(token, id).await?.is_some() {
            return Ok(true);
        }
        match self.get_edge_visible(token, id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) | Err(RuntimeError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Returns `true` if `id` resolves to a live substrate record, by ID, with
    /// no namespace filter.
    ///
    /// Used from `annotates` endpoint validation (`link` and `create`'s
    /// `annotates` targets), which consume a by-ID endpoint and so must follow
    /// the same namespace-agnostic by-ID contract as `get()`.
    pub(crate) async fn substrate_exists_by_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<bool> {
        if self.resolve_edge_endpoint(token, id).await?.is_some() {
            return Ok(true);
        }
        match self.get_edge(token, id).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) | Err(RuntimeError::NotFound(_)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Find the newest live annotation note with an exact kind and tag across
    /// the token's visible edge namespaces, on this runtime's bound backend.
    /// Each store selects one eligible candidate before returning; note bodies
    /// and the complete annotation history are never hydrated here.
    pub async fn latest_annotating_note(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        kind: &str,
        tag: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.latest_annotating_note_inner(token, node_id, kind, tag, None)
            .await
    }

    /// Select a latest annotation only after its exact top-level string
    /// property has been checked by the bound store.
    pub async fn latest_annotating_note_with_property(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        kind: &str,
        tag: &str,
        property_key: &str,
        property_value: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.latest_annotating_note_inner(
            token,
            node_id,
            kind,
            tag,
            Some((property_key, property_value)),
        )
        .await
    }

    async fn latest_annotating_note_inner(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        kind: &str,
        tag: &str,
        required_property: Option<(&str, &str)>,
    ) -> RuntimeResult<Option<Uuid>> {
        if !self.substrate_exists_in_ns(token, node_id).await? {
            return Ok(None);
        }
        let mut latest: Option<(Uuid, i64)> = None;
        for namespace in token.visible_namespaces() {
            let scoped = NamespaceToken::for_namespace(namespace.clone());
            let graph = self.graph(&scoped)?;
            let candidate = match required_property {
                Some((key, value)) => {
                    graph
                        .latest_annotating_note_with_property(node_id, kind, tag, key, value)
                        .await?
                }
                None => graph.latest_annotating_note(node_id, kind, tag).await?,
            };
            if let Some(candidate) = candidate {
                if latest.is_none_or(|(id, created_at)| {
                    candidate.1 > created_at || (candidate.1 == created_at && candidate.0 < id)
                }) {
                    latest = Some(candidate);
                }
            }
        }
        Ok(latest.map(|(id, _)| id))
    }

    /// Get immediate neighbors of a node, optionally filtered by relation type.
    ///
    /// Pass `relations: Some(vec![EdgeRelation::Annotates])` to retrieve only
    /// annotation edges, enabling cross-substrate navigation.
    ///
    /// Symmetric relations (`competes_with`, `composed_with`) are stored
    /// with the canonical source as the lower UUID. Direction normalization is
    /// applied in `neighbors_with_query` so both callers see correct results.
    pub async fn neighbors(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        direction: Direction,
        limit: Option<u32>,
        relations: Option<Vec<EdgeRelation>>,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_with_query(
            token,
            node_id,
            NeighborQuery {
                direction,
                relations,
                limit,
                min_weight: None,
            },
        )
        .await
    }

    /// Get neighbors with full query control (includes `min_weight`).
    ///
    /// Applies symmetric-relation direction normalization: if the
    /// relations filter contains only symmetric relations the direction is
    /// overridden to `Both` so edges stored in canonical order are always found.
    ///
    /// Soft-deleted entity nodes are excluded from results unless the caller
    /// explicitly requested them (future: `include_deleted` flag; currently
    /// always false).
    pub async fn neighbors_with_query(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_with_query_page(token, node_id, query, None, None, true)
            .await
    }

    /// Get a deterministic neighbor page, optionally applying a continuation
    /// cursor and filtering entity/note kinds before the storage limit.
    /// `enrich` is false for the lightweight edge projection.
    pub async fn neighbors_with_query_page(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
        after: Option<NeighborCursor>,
        neighbor_kinds: Option<Vec<String>>,
        enrich: bool,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        // A full-UUID anchor follows get's by-ID lookup. Only the adjacency
        // expansion below is scoped to the caller's visible namespaces.
        if !self.substrate_exists_by_id(token, node_id).await? {
            return Err(RuntimeError::NotFound(format!(
                "neighbor anchor {node_id} not found"
            )));
        }

        self.neighbors_for_resolved_kg_read(
            token,
            node_id,
            crate::KgNeighborRead {
                query,
                after,
                neighbor_kinds,
                enrich,
                namespace: None,
            },
        )
        .await
    }

    /// Expand an already resolved live KG origin on this runtime's graph.
    ///
    /// The caller must verify the origin's existence and apply any record-kind
    /// read scope before calling. The original caller token is retained for
    /// namespace selection and enrichment; an optional namespace may only
    /// narrow its visible set. Like the ordinary neighbor read, resolution and
    /// adjacency are separate reads rather than an atomic record snapshot.
    pub async fn neighbors_for_resolved_kg_read(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        options: crate::KgNeighborRead,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        self.neighbors_for_resolved_kg_read_inner(token, node_id, options, false)
            .await
            .map(|(hits, _)| hits)
    }

    /// Expand a resolved origin and return visible live entity-kind hints for
    /// mailbox endpoint checks. Lightweight projections obtain these hints in
    /// the existing deletion-screen read, without enriching the returned hits.
    /// Missing hints still require the owning message-note backend's policy read.
    pub async fn neighbors_for_resolved_kg_read_with_entity_kinds(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        options: crate::KgNeighborRead,
    ) -> RuntimeResult<(Vec<NeighborHit>, HashMap<Uuid, String>)> {
        self.neighbors_for_resolved_kg_read_inner(token, node_id, options, true)
            .await
    }

    async fn neighbors_for_resolved_kg_read_inner(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        options: crate::KgNeighborRead,
        with_entity_kinds: bool,
    ) -> RuntimeResult<(Vec<NeighborHit>, HashMap<Uuid, String>)> {
        let crate::KgNeighborRead {
            mut query,
            after,
            neighbor_kinds,
            enrich,
            namespace,
        } = options;
        let namespaces = crate::kg_read::neighbor_read_namespaces(token, namespace.as_ref())?;
        query.direction =
            normalize_symmetric_direction(query.direction, query.relations.as_deref());
        let mut hits = Vec::new();
        for ns in namespaces {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_hits = self
                .graph(&temp)?
                .neighbors_page(node_id, query.clone(), after, neighbor_kinds.clone())
                .await?;
            hits.append(&mut ns_hits);
        }
        hits.sort_by_key(|h| (h.node_id, h.edge_id));
        hits.dedup_by_key(|h| (h.node_id, h.edge_id));
        if enrich {
            self.enrich_neighbor_hits(token, &mut hits).await;
        }
        // Filter out soft-deleted entity nodes.
        let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.node_id).collect();
        let (deleted, entity_kinds) = self
            .neighbor_node_screen(
                candidate_ids,
                (with_entity_kinds && !enrich).then_some(token),
            )
            .await?;
        if !deleted.is_empty() {
            hits.retain(|h| !deleted.contains(&h.node_id));
        }
        // Restore the weight-descending, node_id-ascending order the storage
        // layer established (khive-db graph.rs `ORDER BY weight DESC, node_id
        // ASC`) — the (node_id, edge_id) sort above exists only to make
        // `dedup_by_key` adjacent-comparable and otherwise discards it. This
        // ordering contract must hold at every call site of this op (context
        // and neighbors verb alike), for every direction.
        hits.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.node_id.cmp(&b.node_id))
                .then(a.edge_id.cmp(&b.edge_id))
        });
        Ok((hits, entity_kinds))
    }

    /// Find live `annotates` edges targeting one record without applying a
    /// namespace predicate.
    ///
    /// This is the graph counterpart to the namespace-agnostic by-ID `get`
    /// contract (ADR-007 Rev 6). Multi-record neighbor traversal remains
    /// visibility-scoped; callers should use this only after resolving a live
    /// target through a by-ID operation.
    pub async fn annotation_neighbors_by_target_id(
        &self,
        target_id: Uuid,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        let mut reader = self.sql().reader().await?;
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT source_id, id, weight FROM graph_edges \
                      WHERE target_id = ?1 AND relation = ?2 AND deleted_at IS NULL \
                      ORDER BY weight DESC, source_id ASC"
                    .to_string(),
                params: vec![
                    SqlValue::Text(target_id.to_string()),
                    SqlValue::Text(EdgeRelation::Annotates.to_string()),
                ],
                label: Some("annotations.by_target_id_unfiltered".into()),
            })
            .await?;

        rows.into_iter()
            .map(|row| {
                let parse_uuid = |name: &str| match row.get(name) {
                    Some(SqlValue::Text(value)) => Uuid::from_str(value).map_err(|error| {
                        RuntimeError::Internal(format!("graph_edges.{name} is not a UUID: {error}"))
                    }),
                    Some(value) => Err(RuntimeError::Internal(format!(
                        "graph_edges.{name} has unexpected SQL value {value:?}"
                    ))),
                    None => Err(RuntimeError::Internal(format!(
                        "graph_edges row missing {name}"
                    ))),
                };
                let weight = match row.get("weight") {
                    Some(SqlValue::Float(value)) => Ok(*value),
                    Some(value) => Err(RuntimeError::Internal(format!(
                        "graph_edges.weight has unexpected SQL value {value:?}"
                    ))),
                    None => Err(RuntimeError::Internal(
                        "graph_edges row missing weight".into(),
                    )),
                }?;

                Ok(NeighborHit {
                    node_id: parse_uuid("source_id")?,
                    edge_id: parse_uuid("id")?,
                    relation: EdgeRelation::Annotates,
                    weight,
                    name: None,
                    kind: None,
                    entity_type: None,
                })
            })
            .collect()
    }

    /// Get both-direction neighbors, each tagged with the direction (`Out`/
    /// `In`) it was found in, via a single storage query per visible
    /// namespace instead of two separate direction-scoped `neighbors_with_query`
    /// calls: halving the neighbor SELECT count for `context(direction="both")`
    /// expansion. `query.direction` is ignored: always both.
    ///
    /// Mirrors `neighbors_with_query`'s dedup/enrich/soft-delete-filter/order
    /// pipeline exactly, carrying the per-hit direction tag through unchanged.
    pub async fn neighbors_with_query_directed(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
    ) -> RuntimeResult<Vec<(NeighborHit, Direction)>> {
        if !self.substrate_exists_by_id(token, node_id).await? {
            return Err(RuntimeError::NotFound(format!(
                "neighbor anchor {node_id} not found"
            )));
        }

        self.directed_neighbors_for_resolved_kg_read(token, node_id, query, None)
            .await
    }

    pub(crate) async fn directed_neighbors_for_resolved_kg_read(
        &self,
        token: &NamespaceToken,
        node_id: Uuid,
        query: NeighborQuery,
        namespace: Option<&crate::Namespace>,
    ) -> RuntimeResult<Vec<(NeighborHit, Direction)>> {
        let namespaces = crate::kg_read::neighbor_read_namespaces(token, namespace)?;
        let mut hits: Vec<DirectedNeighborHit> = Vec::new();
        for ns in namespaces {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_hits = self
                .graph(&temp)?
                .neighbors_both_directions(node_id, query.clone())
                .await?;
            hits.append(&mut ns_hits);
        }
        // Direction is part of the key (not just node_id/edge_id) so a
        // self-loop's Out row and In row — same node_id and edge_id, opposite
        // direction: sort adjacent but distinct and both survive dedup.
        hits.sort_by_key(|h| {
            (
                h.hit.node_id,
                h.hit.edge_id,
                direction_sort_rank(&h.direction),
            )
        });
        hits.dedup_by_key(|h| {
            (
                h.hit.node_id,
                h.hit.edge_id,
                direction_sort_rank(&h.direction),
            )
        });

        let mut plain_hits: Vec<NeighborHit> = hits.iter().map(|h| h.hit.clone()).collect();
        self.enrich_neighbor_hits(token, &mut plain_hits).await;
        for (dh, enriched) in hits.iter_mut().zip(plain_hits) {
            dh.hit = enriched;
        }

        // Filter out soft-deleted entity nodes.
        let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.hit.node_id).collect();
        let deleted = self.deleted_entity_ids(candidate_ids).await?;
        if !deleted.is_empty() {
            hits.retain(|h| !deleted.contains(&h.hit.node_id));
        }
        // Same global weight-descending/node_id-ascending restore as
        // `neighbors_with_query`: the (node_id, edge_id, direction) sort above
        // exists only to make `dedup_by_key` adjacent-comparable.
        hits.sort_by(|a, b| {
            b.hit
                .weight
                .partial_cmp(&a.hit.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.hit.node_id.cmp(&b.hit.node_id))
                .then(a.hit.edge_id.cmp(&b.hit.edge_id))
        });
        Ok(hits.into_iter().map(|h| (h.hit, h.direction)).collect())
    }

    /// Traverse the graph from a set of root nodes.
    ///
    /// Full-UUID roots use the by-ID contract; expansion and returned edges
    /// remain scoped to the caller's visible namespaces. Missing roots refuse.
    /// Soft-deleted entity nodes are excluded from results.
    pub async fn traverse(
        &self,
        token: &NamespaceToken,
        request: TraversalRequest,
    ) -> RuntimeResult<Vec<GraphPath>> {
        let mut request = request;
        request.validate().map_err(RuntimeError::InvalidInput)?;
        let mut roots = Vec::with_capacity(request.roots.len());
        let mut seen_roots = std::collections::HashSet::with_capacity(request.roots.len());
        for root in request.roots.drain(..) {
            if seen_roots.insert(root) {
                if !self.substrate_exists_by_id(token, root).await? {
                    return Err(RuntimeError::NotFound(format!(
                        "traverse root {root} not found"
                    )));
                }
                roots.push(root);
            }
        }
        request.roots = roots;
        if request.roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut paths = Vec::new();
        for ns in token.visible_namespaces() {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_paths = self.graph(&temp)?.traverse(request.clone()).await?;
            paths.append(&mut ns_paths);
        }
        // Reconcile the per-namespace GraphPaths back down to one per
        // distinct root_id (see merge_traversal_paths_by_root for why this
        // is needed and what it enforces).
        let mut paths =
            merge_traversal_paths_by_root(paths, Some(request.options.effective_limit()));
        self.enrich_path_nodes(token, &mut paths, request.include_properties)
            .await;
        // Filter out soft-deleted entity nodes from all path nodes.
        let all_node_ids: Vec<Uuid> = paths
            .iter()
            .flat_map(|p| p.nodes.iter().map(|n| n.node_id))
            .collect();
        let deleted = self.deleted_entity_ids(all_node_ids).await?;
        if !deleted.is_empty() {
            for path in paths.iter_mut() {
                path.nodes.retain(|n| !deleted.contains(&n.node_id));
                recompute_total_weight(path);
            }
            paths.retain(|p| !p.nodes.is_empty());
        }
        Ok(paths)
    }

    /// Batch-query for soft-deleted UUIDs in `ids`, across BOTH the entities
    /// and notes tables.
    ///
    /// Neighbor/traverse candidates can be note-kind nodes (e.g. reached via
    /// `annotates` edges) as well as entities; a screen that only consults
    /// `entities` lets soft-deleted note targets leak through and hydrate as
    /// blank/missing hits. This is a view-layer read-only screen: it does
    /// not touch edges or mutate any data.
    ///
    /// Returns the subset of `ids` that have `deleted_at IS NOT NULL` in
    /// either table. Takes `Vec<Uuid>` (not an iterator) so the async state
    /// machine holds only owned data — no iterator borrow across yields.
    ///
    /// Propagates reader-admission and other storage errors instead of
    /// treating them as "nothing is deleted": on a saturated pool this query
    /// now runs on the bounded reader pool like any other read, and silently
    /// swallowing its failure would let soft-deleted nodes back into
    /// `neighbors`/`traverse` results instead of surfacing the retryable
    /// admission failure those callers otherwise promise.
    async fn deleted_entity_ids(
        &self,
        ids: Vec<Uuid>,
    ) -> RuntimeResult<std::collections::HashSet<Uuid>> {
        self.neighbor_node_screen(ids, None)
            .await
            .map(|(deleted, _)| deleted)
    }

    /// Share the deletion-screen statement with optional live entity-kind
    /// hints. Only the caller's visible entity namespaces supply hints; note
    /// kinds are not inferred from this backend when notes may route elsewhere.
    async fn neighbor_node_screen(
        &self,
        ids: Vec<Uuid>,
        kind_token: Option<&NamespaceToken>,
    ) -> RuntimeResult<(std::collections::HashSet<Uuid>, HashMap<Uuid, String>)> {
        if ids.is_empty() {
            return Ok((std::collections::HashSet::new(), HashMap::new()));
        }
        let id_strs: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        let n = id_strs.len();
        // Each UNION half gets its OWN numbered-placeholder block (?1..?n for
        // entities, ?(n+1)..?(2n) for notes) — numbered SQLite params bind by
        // index, so reusing the same numbers across halves would silently
        // collapse to a single shared block instead of binding the full list
        // twice (see khive-db/src/stores/graph.rs batch_neighbors: "each half
        // is a fully independent positional-parameter block").
        let entities_placeholders = (0..n)
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let notes_placeholders = (0..n)
            .map(|i| format!("?{}", n + i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql_str = if kind_token.is_some() {
            format!(
                "SELECT id, kind, namespace, deleted_at IS NOT NULL AS is_deleted \
                 FROM entities WHERE id IN ({entities_placeholders}) \
                 UNION ALL \
                 SELECT id, NULL, NULL, 1 FROM notes \
                 WHERE id IN ({notes_placeholders}) AND deleted_at IS NOT NULL"
            )
        } else {
            format!(
                "SELECT id FROM entities WHERE id IN ({entities_placeholders}) AND deleted_at IS NOT NULL \
                 UNION \
                 SELECT id FROM notes WHERE id IN ({notes_placeholders}) AND deleted_at IS NOT NULL"
            )
        };
        // Same id list bound twice — once per UNION arm's independent placeholder block.
        let params: Vec<SqlValue> = id_strs
            .iter()
            .chain(id_strs.iter())
            .cloned()
            .map(SqlValue::Text)
            .collect();
        let stmt = SqlStatement {
            sql: sql_str,
            params,
            label: Some("deleted_entity_ids".into()),
        };
        let mut out = std::collections::HashSet::new();
        let mut entity_kinds = HashMap::new();
        let sql = self.sql();
        let mut reader = sql.reader().await?;
        let rows = reader.query_all(stmt).await?;
        for row in rows {
            if let Some(col) = row.columns.first() {
                if let SqlValue::Text(s) = &col.value {
                    if let Ok(u) = s.parse::<Uuid>() {
                        if kind_token.is_none()
                            || matches!(
                                row.columns.get(3).map(|col| &col.value),
                                Some(SqlValue::Integer(1))
                            )
                        {
                            out.insert(u);
                        } else if let (
                            Some(token),
                            Some(SqlValue::Text(kind)),
                            Some(SqlValue::Text(namespace)),
                            Some(SqlValue::Integer(0)),
                        ) = (
                            kind_token,
                            row.columns.get(1).map(|col| &col.value),
                            row.columns.get(2).map(|col| &col.value),
                            row.columns.get(3).map(|col| &col.value),
                        ) {
                            if token
                                .visible_namespaces()
                                .iter()
                                .any(|ns| ns.as_str() == namespace.as_str())
                            {
                                entity_kinds.insert(u, kind.clone());
                            }
                        }
                    }
                }
            }
        }
        Ok((out, entity_kinds))
    }

    /// Populate `name` and `kind` on each `NeighborHit` from the corresponding
    /// entity or note record. Best-effort: unresolved IDs leave the fields `None`.
    ///
    /// Uses a single batched entity lookup via `get_entities_by_ids_visible`
    /// (scoped to the token's full visible-namespace set so that neighbors in
    /// extra-visible namespaces are enriched), then a batched note lookup
    /// (`get_notes_batch`) for the residual IDs not resolved as entities.
    /// Order and identity of hits is preserved via `HashMap` re-index.
    async fn enrich_neighbor_hits(&self, token: &NamespaceToken, hits: &mut [NeighborHit]) {
        if hits.is_empty() {
            return;
        }

        // Deduplicated IDs for the batch call.
        let unique_ids: Vec<Uuid> = {
            let mut seen = std::collections::HashSet::new();
            hits.iter()
                .filter_map(|h| {
                    if seen.insert(h.node_id) {
                        Some(h.node_id)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let entity_map: HashMap<Uuid, Entity> = self
            .get_entities_by_ids_visible(token, &unique_ids)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.id, e))
            .collect();

        // Batch note lookup for IDs not found as entities.
        let residual_ids: Vec<Uuid> = unique_ids
            .iter()
            .filter(|id| !entity_map.contains_key(id))
            .copied()
            .collect();

        let note_map: HashMap<Uuid, Note> = if !residual_ids.is_empty() {
            if let Ok(store) = self.notes(token) {
                store
                    .get_notes_batch(&residual_ids)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect()
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

        for hit in hits.iter_mut() {
            if let Some(entity) = entity_map.get(&hit.node_id) {
                hit.name = Some(entity.name.clone());
                hit.kind = Some(entity.kind.clone());
                hit.entity_type = entity.entity_type.clone();
            } else if let Some(note) = note_map.get(&hit.node_id) {
                hit.name = Some(note_graph_name(note));
                hit.kind = Some(note.kind.clone());
            }
        }
    }

    /// Populate `name` and `kind` on each `PathNode` from the corresponding
    /// entity or note record. Same best-effort policy as `enrich_neighbor_hits`.
    ///
    /// Uses `get_entities_by_ids_visible` so that path nodes whose entities
    /// live in extra-visible namespaces are enriched correctly. Node IDs that
    /// repeat across paths are fetched exactly once.
    ///
    /// `include_properties` gates whether `entity.properties` is cloned onto
    /// each node. When `false` (the default), the potentially large JSON blob
    /// is never read from the map, keeping the hot path allocation-free.
    async fn enrich_path_nodes(
        &self,
        token: &NamespaceToken,
        paths: &mut [GraphPath],
        include_properties: bool,
    ) {
        if paths.is_empty() {
            return;
        }

        // Deduplicate node IDs across all paths before the batch call.
        let unique_ids: Vec<Uuid> = {
            let mut seen = std::collections::HashSet::new();
            paths
                .iter()
                .flat_map(|p| p.nodes.iter())
                .filter_map(|n| {
                    if seen.insert(n.node_id) {
                        Some(n.node_id)
                    } else {
                        None
                    }
                })
                .collect()
        };

        let entity_map: HashMap<Uuid, Entity> = self
            .get_entities_by_ids_visible(token, &unique_ids)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.id, e))
            .collect();

        let residual_ids: Vec<Uuid> = unique_ids
            .iter()
            .filter(|id| !entity_map.contains_key(id))
            .copied()
            .collect();

        let note_map: HashMap<Uuid, Note> = if !residual_ids.is_empty() {
            if let Ok(store) = self.notes(token) {
                store
                    .get_notes_batch(&residual_ids)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|n| (n.id, n))
                    .collect()
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

        for path in paths.iter_mut() {
            for node in path.nodes.iter_mut() {
                if let Some(entity) = entity_map.get(&node.node_id) {
                    node.name = Some(entity.name.clone());
                    node.kind = Some(entity.kind.clone());
                    if include_properties {
                        node.properties = entity.properties.clone();
                    }
                } else if let Some(note) = note_map.get(&node.node_id) {
                    node.name = Some(note_graph_name(note));
                    node.kind = Some(note.kind.clone());
                }
            }
        }
    }

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
        self.try_create_note_impl(token, kind, name, content, properties, false, None, None)
            .await
    }

    /// Like [`Self::try_create_note`] but permits the caller to establish the
    /// transport-owned `message` properties that `try_create_note` refuses.
    ///
    /// This is a deliberately named, separate entry point rather than a flag
    /// on `try_create_note` so the trust decision is visible at every call
    /// site: `comm.ingest` (`khive-pack-comm/src/handlers/ingest.rs`) is the sole
    /// legitimate caller, because it is the only code that has just derived
    /// quarantine disposition and channel provenance from the inbound
    /// transport itself. The caller set is bounded by possession, not
    /// documentation: the required [`crate::ChannelIngestCapability`] is
    /// constructible only inside this crate and granted at pack registration
    /// exclusively to channel-transport packs. Every other write path uses
    /// `try_create_note`, which rejects those properties
    /// unconditionally.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_create_note_as_trusted_ingest(
        &self,
        _capability: &crate::pack::ChannelIngestCapability,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        expires_after: Option<std::time::Duration>,
    ) -> RuntimeResult<Option<Note>> {
        self.try_create_note_impl(
            token,
            kind,
            name,
            content,
            properties,
            true,
            None,
            expires_after,
        )
        .await
    }

    /// Publish a trusted inbound message and its original-byte attachment in
    /// one database transaction. Channel quarantine must not advertise a
    /// reference in note metadata before GC can see its attachment owner.
    #[allow(clippy::too_many_arguments)]
    pub async fn try_create_note_as_trusted_ingest_with_attachment(
        &self,
        _capability: &crate::pack::ChannelIngestCapability,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
        attachment: NewAttachment,
        expires_after: Option<std::time::Duration>,
    ) -> RuntimeResult<Option<Note>> {
        self.try_create_note_impl(
            token,
            kind,
            name,
            content,
            properties,
            true,
            Some(attachment),
            expires_after,
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
    ) -> RuntimeResult<Option<Note>> {
        self.validate_note_kind(kind)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        crate::secret_gate::check_at(content, "note", "content")?;
        if let Some(n) = name {
            crate::secret_gate::check_at(n, "note", "name")?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json_at(p, "note", "properties")?;
        }
        if let Some(ref attachment) = attachment {
            // The note and its owner row must share the main database. A
            // secondary pack backend cannot atomically root the reference.
            drop(self.attachments()?);
            attachment.validate()?;
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
        crate::secret_gate::check_at(content, "note", "content")?;
        if let Some(n) = name {
            crate::secret_gate::check_at(n, "note", "name")?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json_at(p, "note", "properties")?;
        }
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

    /// List notes visible to the token, optionally filtered by kind.
    ///
    /// When the token carries a multi-namespace visible set, notes from all
    /// visible namespaces are returned. When the visible set is `[primary]`
    /// (the default) this behaves identically to the pre-visibility behaviour.
    pub async fn list_notes(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Note>> {
        let visible = token.visible_namespaces();
        if visible.len() == 1 {
            // Fast path: single namespace — use the dedicated query_notes method.
            let page = self
                .notes(token)?
                .query_notes_count_free(
                    token.namespace().as_str(),
                    kind,
                    PageRequest {
                        offset: offset.into(),
                        limit,
                    },
                )
                .await?;
            return Ok(page.items);
        }
        // Multi-namespace path: use query_notes_filtered with the visible set.
        use khive_storage::note::NoteFilter;
        let ns_strs: Vec<String> = visible.iter().map(|ns| ns.as_str().to_owned()).collect();
        let filter = NoteFilter {
            kind: kind.map(|k| k.to_string()),
            namespaces: ns_strs,
            ..Default::default()
        };
        let page = self
            .notes(token)?
            .query_notes_filtered_count_free(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// List an immutable insertion-sequence page of visible notes.
    ///
    /// Soft-deleting the prior page's last note does not invalidate the
    /// cursor because the boundary is resolved including tombstones. A hard
    /// deletion makes the cursor unresolvable and returns an explicit error.
    pub async fn list_notes_after(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Note>, Option<Uuid>)> {
        let store = self.notes(token)?;
        let after = match after {
            Some(id) => {
                let note = self
                    .get_note_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("note cursor {id}")))?;
                Self::ensure_namespace_visible(&note.namespace, token)?;
                let sequence = store.note_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "note cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
            None => None,
        };
        let filter = khive_storage::note::NoteFilter {
            kind: kind.map(str::to_string),
            namespaces: token
                .visible_namespaces()
                .iter()
                .map(|namespace| namespace.as_str().to_owned())
                .collect(),
            ..Default::default()
        };
        let page = store
            .query_notes_filtered_after(token.namespace().as_str(), &filter, after, limit)
            .await?;
        Ok((page.items, page.next_after.map(|cursor| cursor.id)))
    }

    /// Count notes matching `kind` across the caller's visible namespaces.
    pub async fn count_notes(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        Ok(self
            .notes(token)?
            .count_notes_in_namespaces(&namespaces, kind)
            .await?)
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

    #[allow(clippy::too_many_arguments)]
    async fn search_notes_inner(
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
        tolerate_vector_error: bool,
    ) -> RuntimeResult<(Vec<NoteSearchHit>, Option<String>)> {
        const RRF_K: usize = 60;
        let candidates = limit.saturating_mul(4).max(limit);
        let visible_ns: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();

        // FTS5 over the notes index — search all visible namespaces.
        //
        // `sanitize_fts5_query` strips known-unsafe FTS5 metacharacters, but
        // residual punctuation the sanitizer does not strip can still reach
        // the FTS5 parser and error. This fails loud instead of degrading to
        // vector-only fusion, so callers see the bad query instead of
        // silently losing the lexical leg. Errors from any other leg (vector
        // search, note hydration) still propagate normally.
        //
        // Injection: check FTS_SEARCH_FAIL_NS (armed by `arm_fts_search_fail(ns)`),
        // exercising the propagate branch above. Fires only when the armed
        // namespace is among this call's visible namespaces, then clears (one-shot).
        #[cfg(any(test, feature = "fault-injection"))]
        let fts_search_inject = {
            let mut g = FTS_SEARCH_FAIL_NS.lock().unwrap();
            match g.as_deref() {
                Some(armed) if visible_ns.iter().any(|ns| ns == armed) => {
                    *g = None;
                    true
                }
                _ => false,
            }
        };
        #[cfg(not(any(test, feature = "fault-injection")))]
        let fts_search_inject = false;

        let text_store = self.text_for_notes(token)?;
        let text_fut = async {
            if fts_search_inject {
                return Err(khive_storage::StorageError::Timeout {
                    operation: "fts_search".into(),
                });
            }
            text_store
                .search(TextSearchRequest {
                    query: query_text.to_string(),
                    mode: text_mode,
                    filter: Some(TextFilter {
                        namespaces: visible_ns.clone(),
                        // Push the note-kind filter into the FTS query. Without it the
                        // text arm returns the top `candidates` rows across EVERY note
                        // kind in the namespace and the kind is applied post-fetch, so a
                        // store where short message/session rows outrank task
                        // descriptions under BM25 hands the caller one or two task hits
                        // while the store holds many more carrying the literal.
                        record_kinds: note_kind
                            .map(|kind| vec![kind.to_string()])
                            .unwrap_or_default(),
                        ..TextFilter::default()
                    }),
                    top_k: candidates,
                    snippet_chars: 200,
                })
                .await
        };
        let text_fut = crate::stage_seam::text_stage(text_fut);

        // Vector search filtered to notes; it runs with the text stage, and a text error wins.
        let vector_fut = async {
            if query_vector.is_some() || self.config().embedding_model.is_some() {
                self.note_search_vector_search(token, query_vector, query_text, candidates)
                    .await
            } else {
                Ok(vec![])
            }
        };
        let (text_search_result, vector_result) = tokio::join!(text_fut, vector_fut);

        // FtsPasses is counted inside the store's `search()` (khive-db
        // stores/text.rs), only once a real FTS5 statement is prepared —
        // an empty/fully-sanitized query short-circuits there before any
        // statement exists and must not count (nor does the injected-failure
        // branch above, which never reaches the store at all).
        let text_hits = crate::error::fts_text_leg_or_err(
            text_search_result.map_err(RuntimeError::from),
            "search_notes",
            query_text,
        )?;

        let mut vector_error: Option<String> = None;
        let vector_hits = match vector_result {
            Ok(hits) => hits,
            Err(e) if tolerate_vector_error => {
                vector_error = Some(e.to_string());
                Vec::new()
            }
            Err(e) => return Err(e),
        };

        // Keep the full text∪vector union through RRF — salience weighting and
        // soft-delete/kind filtering happen *after* this, and the final
        // `hits.truncate(limit)` is the only result-limiting cut. Truncating to
        // `candidates` here would drop a high-salience note ranked just outside
        // the raw RRF cutoff before salience ever applied.
        let fuse_k = text_hits.len() + vector_hits.len();
        let fused = crate::fusion::rrf_fuse_k(self, text_hits, vector_hits, RRF_K, fuse_k).await?;

        let candidate_ids: Vec<Uuid> = fused.iter().map(|hit| hit.entity_id).collect();
        if candidate_ids.is_empty() {
            return Ok((vec![], vector_error));
        }

        // Hydrate every candidate note with one batched read to get salience and
        // apply soft-delete + (optional) kind filtering. The store chunks the read
        // below its bound-parameter ceiling, so the read costs `ceil(candidates / 900)`
        // statements instead of one per candidate. Notes whose `kind` doesn't
        // match `note_kind` are dropped post-fetch — they're a small set
        // bounded by the text∪vector union (≤ 2×candidates), so the read is cheap.
        let note_store = self.notes(token)?;
        let search_pool = self.backend().pool_arc();
        let mailbox_view = crate::MailboxView {
            actor_id: token.actor().id.clone(),
            delegated: false,
        };
        let mut alive_notes: HashMap<Uuid, Note> = HashMap::new();
        for note in note_store.get_notes_batch(&candidate_ids).await? {
            search_pool.record_note_candidate_hydration_row();
            if note.deleted_at.is_some() {
                continue;
            }
            if !mailbox_view.permits_message_note(token, &note) {
                continue;
            }
            if let Some(want_kind) = note_kind {
                if note.kind != want_kind {
                    continue;
                }
            }
            // Apply tag predicate before adding to alive set: tags on notes live
            // inside `properties["tags"]` (a JSON array). This pushes the filter
            // before truncation so matching notes ranked beyond `limit` in the raw
            // fusion are not silently dropped.
            if !tags_any.is_empty() {
                let note_tags: Vec<String> = note
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("tags"))
                    .and_then(serde_json::Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if !note_tags
                    .iter()
                    .any(|t| tags_any.iter().any(|w| t.eq_ignore_ascii_case(w)))
                {
                    continue;
                }
            }
            // Apply properties predicate before truncation, same reasoning as tags above.
            if let Some(pf) = properties_filter {
                if !note_props_match(note.properties.as_ref(), pf) {
                    continue;
                }
            }
            alive_notes.insert(note.id, note);
        }

        // Drop superseded notes unless include_superseded is true: any note targeted
        // by a `supersedes` edge is obsolete and excluded from default search.
        if !include_superseded && !alive_notes.is_empty() {
            let graph = self.graph(token)?;
            let note_ids: Vec<Uuid> = alive_notes.keys().copied().collect();
            let superseded: std::collections::HashSet<Uuid> = graph
                .batch_neighbors(
                    &note_ids,
                    NeighborQuery {
                        direction: Direction::In,
                        relations: Some(vec![EdgeRelation::Supersedes]),
                        limit: Some(1),
                        min_weight: None,
                    },
                )
                .await?
                .into_iter()
                .map(|(note_id, _)| note_id)
                .collect();
            alive_notes.retain(|id, _| !superseded.contains(id));
        }

        // Apply salience weighting and collect final hits.
        let mut hits: Vec<NoteSearchHit> = fused
            .into_iter()
            .filter_map(|hit| {
                let note = alive_notes.get(&hit.entity_id)?;
                let weighted = salience_weighted_rank(hit.score, note.salience);
                Some(NoteSearchHit {
                    note_id: hit.entity_id,
                    score: weighted,
                    rank_score_kind: hit.rank_score_kind,
                    signals: hit.signals,
                    source: hit.source,
                    title: hit.title.or_else(|| note_title(note)),
                    snippet: hit.snippet.or_else(|| note_snippet(note)),
                })
            })
            .collect();

        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.note_id.cmp(&b.note_id)));
        hits.truncate(limit as usize);
        Ok((hits, vector_error))
    }

    /// Parse a full UUID or resolve a compact hexadecimal prefix in the caller's
    /// primary namespace, using the same lookup policy as [`Self::resolve_prefix`].
    ///
    /// A parseable full UUID is returned without a lookup or an existence,
    /// liveness or authorization check. Otherwise input must contain at least
    /// eight ASCII hexadecimal characters with no separators or whitespace.
    /// Missing prefixes and invalid shapes return distinct `InvalidInput` errors;
    /// lookup storage and ambiguity errors propagate unchanged.
    pub async fn resolve_uuid_or_prefix(
        &self,
        token: &NamespaceToken,
        s: &str,
    ) -> RuntimeResult<Uuid> {
        if let Ok(uuid) = s.parse::<Uuid>() {
            return Ok(uuid);
        }
        if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            return match self.resolve_prefix(token, s).await? {
                Some(uuid) => Ok(uuid),
                None => Err(RuntimeError::InvalidInput(format!(
                    "no record matches prefix: {s:?}"
                ))),
            };
        }
        Err(RuntimeError::InvalidInput(format!(
            "invalid UUID (expected full UUID or 8+ hex prefix): {s:?}"
        )))
    }

    /// Resolve a short UUID prefix (8+ hex chars) to a full UUID.
    ///
    /// Searches entities, notes, and edges tables for a UUID starting with the
    /// given prefix, scoped to the caller's primary namespace only. Returns
    /// `Ok(Some(uuid))` if exactly one match is found, `Ok(None)` if no
    /// matches, or an error if ambiguous (multiple matches).
    pub async fn resolve_prefix(
        &self,
        token: &NamespaceToken,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        let namespaces = [token.namespace().as_str().to_owned()];
        self.resolve_prefix_inner(Some(&namespaces), prefix, false, false)
            .await
    }

    pub async fn resolve_prefix_including_deleted(
        &self,
        token: &NamespaceToken,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        let namespaces = [token.namespace().as_str().to_owned()];
        self.resolve_prefix_inner(Some(&namespaces), prefix, true, false)
            .await
    }

    /// Resolve a short UUID prefix (8+ hex chars) to a full UUID with NO
    /// namespace filter at all: mirrors `resolve_by_id`'s by-ID contract:
    /// by-ID resolution is namespace-agnostic, since the Gate (not
    /// storage-layer filtering) is the authz seam. Used by the four by-ID
    /// CRUD verbs (get/update/delete/merge) so their prefix path matches
    /// their already-unfiltered full-UUID path. No token param: unlike
    /// `resolve_prefix`, there is no namespace to derive from one.
    pub async fn resolve_prefix_unfiltered(&self, prefix: &str) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, false, false).await
    }

    /// `resolve_prefix_unfiltered`, including soft-deleted rows — used by the
    /// hard-delete by-ID path.
    pub async fn resolve_prefix_unfiltered_including_deleted(
        &self,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, true, false).await
    }

    /// The configured multi-backend read inventory has completed base schema
    /// bootstrap. A missing table is a backend failure there, not an absent ID.
    pub(crate) async fn resolve_prefix_for_kg_read(
        &self,
        prefix: &str,
        include_deleted: bool,
    ) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, include_deleted, true)
            .await
    }

    /// Shared indexed prefix-range lookup over an explicit namespace set.
    ///
    /// `namespaces` selects the lookup scope: `Some(&[ns])` reproduces the
    /// historical primary-only behaviour (`resolve_prefix` /
    /// `resolve_prefix_including_deleted`); `None` applies
    /// no namespace predicate at all (`resolve_prefix_unfiltered*`).
    /// Ambiguity (a prefix matching more than one UUID, even across
    /// different namespaces in the set, or across all namespaces when
    /// unfiltered) is still an error: UUIDs are globally unique, so two
    /// distinct rows sharing a prefix always requires caller disambiguation —
    /// no cross-namespace dedup is needed or performed.
    async fn resolve_prefix_inner(
        &self,
        namespaces: Option<&[String]>,
        prefix: &str,
        include_deleted: bool,
        require_tables: bool,
    ) -> RuntimeResult<Option<Uuid>> {
        // Every caller is expected to pre-validate hex-only input, but this is
        // the single choke point every `resolve_prefix*` variant funnels
        // through, so re-validate here too. A prefix containing anything other
        // than hex digits and canonical hyphen separators (`%`, `_`, or other
        // injection-shaped input) never matches a real id and is rejected
        // before it can reach the range query.
        if !prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Ok(None);
        }

        // Injection: check PREFIX_RESOLVE_FAIL_NS (armed by
        // `arm_prefix_resolve_fail_scoped(prefix)`), exercising the storage-failure path
        // a genuine pool checkout timeout or WAL contention would take.
        #[cfg(any(test, feature = "fault-injection"))]
        if consume_fault(&PREFIX_RESOLVE_FAIL_NS, prefix) {
            return Err(RuntimeError::Storage(
                khive_storage::StorageError::Timeout {
                    operation: "resolve_prefix".into(),
                },
            ));
        }

        let Some((lower, upper)) = uuid_prefix_bounds(prefix) else {
            return Ok(None);
        };

        let tables = [
            ("entities", true),
            ("notes", true),
            ("events", false),
            ("graph_edges", false),
        ];

        // A UUID can legitimately exist in more than one scanned table
        // (e.g. an entity id string that also happens to be an edge id — the
        // lookup is a text-prefix range across independent tables, not a
        // substrate-exclusive lookup). Without dedup, a single record hit
        // twice across tables inflated `matches.len()` past 1 and produced a
        // false `AmbiguousPrefix` naming the SAME UUID twice. `seen` tracks
        // UUIDs already pushed so `matches` (and thus every length check,
        // including the early-exit below) reflects DISTINCT UUIDs only.
        let mut matches: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut reader = self.sql().reader().await.map_err(RuntimeError::Storage)?;

        for (table, has_deleted_at) in tables {
            let sql = resolve_prefix_statement(
                table,
                has_deleted_at,
                include_deleted,
                namespaces,
                &lower,
                &upper,
            );
            match reader.query_all(sql).await {
                Ok(rows) => {
                    for row in rows {
                        if let Some(col) = row.columns.first() {
                            if let SqlValue::Text(s) = &col.value {
                                if seen.insert(s.clone()) {
                                    matches.push(s.clone());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !require_tables && msg.contains("no such table") {
                        continue;
                    }
                    return Err(RuntimeError::Storage(e));
                }
            }
            if matches.len() > 1 {
                break;
            }
        }

        // Sidecar-resident event rows (events-split lane) are invisible to the
        // main-store scan above; a prefix naming one must still resolve, and a
        // prefix colliding across the two files must still read as ambiguous,
        // so the sidecar scan merges into the same `matches`/`seen` set.
        if matches.len() <= 1 {
            if let Some(sidecar_sql) = self.events_sidecar_sql_read_only()? {
                // The main-store statement, so the sidecar's candidates come back in
                // the same id order rather than whichever index the planner picks.
                let mut sql = resolve_prefix_statement(
                    "events",
                    false,
                    include_deleted,
                    namespaces,
                    &lower,
                    &upper,
                );
                sql.label = Some("resolve_prefix.events_sidecar".into());
                let mut sidecar_reader =
                    sidecar_sql.reader().await.map_err(RuntimeError::Storage)?;
                match sidecar_reader.query_all(sql).await {
                    Ok(rows) => {
                        for row in rows {
                            if let Some(col) = row.columns.first() {
                                if let SqlValue::Text(s) = &col.value {
                                    if seen.insert(s.clone()) {
                                        matches.push(s.clone());
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if require_tables || !msg.contains("no such table") {
                            return Err(RuntimeError::Storage(e));
                        }
                    }
                }
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => {
                let uuid = Uuid::from_str(&matches[0])
                    .map_err(|e| RuntimeError::Internal(format!("stored UUID is invalid: {e}")))?;
                Ok(Some(uuid))
            }
            _ => {
                let uuids: Vec<uuid::Uuid> = matches
                    .iter()
                    .filter_map(|s| Uuid::from_str(s).ok())
                    .collect();
                Err(RuntimeError::AmbiguousPrefix {
                    prefix: prefix.to_string(),
                    matches: uuids,
                })
            }
        }
    }

    /// Resolve a UUID to its substrate kind with NO namespace filter.
    ///
    /// By-ID contract: UUID v4 is globally unique: by-ID substrate
    /// inference must return the record regardless of caller namespace.  Used by
    /// the public `update` and `delete` verb handlers when no explicit `kind` is
    /// supplied.
    ///
    /// Does NOT consult the visible set or the primary-namespace check.  The
    /// token is still required to route to the correct backend pool but its
    /// namespace value is not used as a filter.
    pub async fn resolve_by_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        // Entity: direct by-UUID fetch (ID-only, no namespace check).
        if let Some(entity) = self.entities(token)?.get_entity(id).await? {
            return Ok(Some(Resolved::Entity(entity)));
        }

        // Note: direct by-UUID fetch (ID-only).
        if let Some(note) = self.notes(token)?.get_note(id).await? {
            return Ok(Some(Resolved::Note(note)));
        }

        // Edges and events are not returned here; the caller's `_` arm handles
        // those with a separate get_edge / get_event check.
        Ok(None)
    }

    /// Resolve a UUID to its substrate kind with NO namespace filter, including
    /// soft-deleted rows.
    ///
    /// Used by the hard-delete path when no explicit `kind` is supplied, so
    /// already-soft-deleted records can still be located by UUID alone.
    pub async fn resolve_by_id_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        // Entity: including soft-deleted, no namespace check.
        if let Some(entity) = self
            .entities(token)?
            .get_entity_including_deleted(id)
            .await?
        {
            return Ok(Some(Resolved::Entity(entity)));
        }

        // Note: including soft-deleted, no namespace check.
        if let Some(note) = self.notes(token)?.get_note_including_deleted(id).await? {
            return Ok(Some(Resolved::Note(note)));
        }

        // Edges and events are not returned here; the caller's `_` arm handles
        // those with a separate get_edge_including_deleted check.
        Ok(None)
    }

    /// Resolve a UUID to its substrate kind by trying entity, then note, then event stores.
    ///
    /// Returns `None` if the UUID is not found in any substrate.
    /// Cost: at most 3 store lookups per call (cheap for v0.1).
    pub async fn resolve(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        // Entity: use the namespace-checked getter (errors on mismatch/absent).
        match self.get_entity(token, id).await {
            Ok(entity) => return Ok(Some(Resolved::Entity(entity))),
            Err(RuntimeError::NotFound(_) | RuntimeError::NamespaceMismatch { .. }) => {}
            Err(e) => return Err(e),
        }

        // Note: storage get_note is ID-only — verify against visible set.
        if let Some(note) = self.notes(token)?.get_note(id).await? {
            if Self::ensure_namespace_visible(&note.namespace, token).is_ok() {
                return Ok(Some(Resolved::Note(note)));
            }
        }

        // Event: storage get_event is ID-only — verify against visible set.
        if let Some(event) = self.events(token)?.get_event(id).await? {
            if Self::ensure_namespace_visible(&event.namespace, token).is_ok() {
                return Ok(Some(Resolved::Event(event)));
            }
        }

        Ok(None)
    }

    /// Resolve a UUID to its substrate kind with NO namespace filter, for edge
    /// endpoint validation.
    ///
    /// `link` and `create`'s `annotates` targets consume by-ID endpoints, so
    /// their existence check must follow the same by-ID contract as `get()`:
    /// by-ID ops are namespace-agnostic: the Gate, not storage-layer
    /// filtering, is the authz seam. Mirrors `resolve_by_id`
    /// (entity + note, unfiltered) and additionally resolves events,
    /// unfiltered, so edge endpoint validation resolves exactly what `get()`
    /// resolves regardless of the caller's namespace.
    pub async fn resolve_edge_endpoint(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        if let Some(resolved) = self.resolve_by_id(token, id).await? {
            return Ok(Some(resolved));
        }
        if let Some(event) = self.events(token)?.get_event(id).await? {
            return Ok(Some(Resolved::Event(event)));
        }
        Ok(None)
    }

    /// Resolve a UUID to its substrate kind using primary-namespace-only enforcement.
    ///
    /// Unlike `resolve`, never consults the visible set. Use from GTD dependency
    /// validation paths where strict primary ownership is required.
    pub async fn resolve_primary(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_primary_inner(token, id, false).await
    }

    /// Resolve a UUID to its substrate kind, including soft-deleted rows.
    ///
    /// Used exclusively by the hard-delete path to locate records that have
    /// already been soft-deleted. Namespace isolation is still enforced.
    pub async fn resolve_including_deleted(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<Resolved>> {
        self.resolve_primary_inner(token, id, true).await
    }

    // Each arm accepts a record only from the caller's primary namespace; a record in a
    // visible-only namespace is refused and the lookup falls through to the next substrate.
    // Tombstone selection applies only to entities and notes; events keep their live lookup.
    async fn resolve_primary_inner(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        include_deleted: bool,
    ) -> RuntimeResult<Option<Resolved>> {
        let ns = token.namespace().as_str();

        let entity = if include_deleted {
            self.entities(token)?
                .get_entity_including_deleted(id)
                .await?
        } else {
            self.entities(token)?.get_entity(id).await?
        };
        if let Some(entity) = entity {
            if Self::ensure_namespace(&entity.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Entity(entity)));
            }
        }

        let note = if include_deleted {
            self.notes(token)?.get_note_including_deleted(id).await?
        } else {
            self.notes(token)?.get_note(id).await?
        };
        if let Some(note) = note {
            if Self::ensure_namespace(&note.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Note(note)));
            }
        }

        if let Some(event) = self.events(token)?.get_event(id).await? {
            if Self::ensure_namespace(&event.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Event(event)));
            }
        }

        Ok(None)
    }

    /// Hard-delete a single graph node (entity, note, or edge-as-node row) AND purge its
    /// incident edges in ONE write transaction — closes a race where a concurrent guarded
    /// write could insert a fresh edge against the endpoint between two separately
    /// committed calls; see docs/operations.md#atomic_hard_delete_with_edge_purge.
    ///
    /// `row_statement` is the exact hard-delete `DELETE` for the target row
    /// (entity, note, or edge). Before the incident-edge purge, the same plan
    /// appends any ADR-002 lineage-loss warnings from the still-present edge
    /// rows, so the warning payload and cascade commit or roll back together.
    /// Returns `Ok(true)` if the row was deleted, `Ok(false)` if it no longer
    /// existed (lost a race with a concurrent delete of the same row).
    async fn atomic_hard_delete_with_edge_purge(
        &self,
        row_statement: SqlStatement,
        node_id: Uuid,
        namespace: &str,
        actor: &str,
        substrate: SubstrateKind,
    ) -> RuntimeResult<bool> {
        let mut statements = vec![PlanStatement {
            statement: row_statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }];
        if matches!(substrate, SubstrateKind::Entity | SubstrateKind::Note) {
            statements.push(PlanStatement {
                statement: khive_db::stores::attachment::delete_record_attachments_statement(
                    node_id,
                    if substrate == SubstrateKind::Entity {
                        AttachmentSubstrate::Entity
                    } else {
                        AttachmentSubstrate::Note
                    },
                ),
                guard: None,
            });
        }
        statements.extend(
            hard_delete_lineage_warning_statements(namespace, actor, node_id, substrate)
                .into_iter()
                .map(|statement| PlanStatement {
                    statement,
                    guard: None,
                }),
        );
        statements.push(PlanStatement {
            statement: purge_incident_edges_statement(node_id),
            guard: None,
        });
        let plan = AtomicOpPlan::Delete(DeletePlan {
            target_id: node_id,
            statements,
            post_commit: PostCommitEffect::None,
        });
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => Ok(true),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::NoteConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::EntityConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::GuardFailed { .. },
                ..
            }) => Ok(false),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::SqlError { message, .. },
                ..
            }) => Err(RuntimeError::Internal(format!(
                "hard delete + edge purge for {node_id} failed: {message}"
            ))),
            Err(e) => Err(RuntimeError::Internal(format!(
                "hard delete + edge purge for {node_id}: atomic unit seam failure: {}",
                e.0
            ))),
        }
    }

    /// Restore an entity tombstone owned by the caller's primary namespace.
    ///
    /// The restore is guarded by both the tombstone's id/namespace and the
    /// current uniqueness state, so a caller cannot resurrect over a newer
    /// live record. Indexes are rebuilt only after the row restore commits.
    pub async fn restore_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<(Entity, bool)>> {
        let Some(entity) = self
            .entities(token)?
            .get_entity_including_deleted(id)
            .await?
        else {
            return Ok(None);
        };
        if entity.namespace != token.namespace().as_str() {
            return Ok(None);
        }
        // A merge tombstone is not a plain soft delete: the source row carries
        // merge provenance and its content already lives on the kept entity.
        // Clearing only `deleted_at` would bring the source back as a live
        // duplicate that still claims to have been merged. Refuse and name
        // the kept id; restore does not undo a merge.
        //
        // The merge check runs before the already-live short cut on purpose:
        // a live row that still carries `merged_into` is what an earlier
        // restore left behind before this guard existed, and answering it
        // "already live" would hide the invariant violation from the one
        // caller who is looking at the row. Name it instead.
        if let Some(kept_id) = entity.merged_into {
            if entity.deleted_at.is_none() {
                return Err(live_merged_entity_refused(id, kept_id));
            }
            return Err(merge_tombstone_restore_refused(id, kept_id));
        }
        if entity.deleted_at.is_none() {
            return Ok(Some((entity, false)));
        }
        let updated_at =
            Utc::now()
                .timestamp_micros()
                .max(entity.updated_at.checked_add(1).ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "entity {id} updated_at is already at i64::MAX and cannot advance"
                    ))
                })?);
        let mut restored = entity;
        restored.deleted_at = None;
        restored.updated_at = updated_at;
        restored.version = restored
            .version
            .checked_add(1)
            .ok_or_else(|| RuntimeError::InvalidInput("entity version overflow".into()))?;
        let mut statements = vec![PlanStatement {
            statement: SqlStatement {
                sql: "UPDATE entities SET deleted_at=NULL, updated_at=?1, version=version+1 \
                      WHERE id=?2 AND namespace=?3 AND deleted_at IS NOT NULL AND version=?4"
                    .into(),
                params: vec![
                    SqlValue::Integer(updated_at),
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(token.namespace().as_str().to_owned()),
                    SqlValue::Integer(restored.version - 1),
                ],
                label: Some("entity-restore".into()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }];
        // The soft delete removed the FTS row, so the text index is published
        // in the same unit as the row: a live row that search cannot find is
        // not a state this verb can leave behind. Order-sensitive pair — see
        // `insert_document_statements`'s adjacency contract.
        for statement in khive_db::stores::text::delete_document_statements(
            "fts_entities",
            &restored.namespace,
            id,
        )
        .into_iter()
        .chain(insert_document_statements(
            "fts_entities",
            &entity_fts_document(&restored),
        )) {
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
                // Embeddings are rebuilt after the commit; the row and its
                // text index are already live, so a failure here names that.
                #[cfg(any(test, feature = "fault-injection"))]
                if consume_fault(&FTS_FAIL_NS, &restored.namespace) {
                    return Err(restore_reindex_failed(
                        "entity",
                        id,
                        RuntimeError::Internal("injected FTS failure".to_string()),
                    ));
                }
                self.reindex_entity(token, &restored)
                    .await
                    .map_err(|e| restore_reindex_failed("entity", id, e))?;
                Ok(Some((restored, true)))
            }
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("entity restore rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
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

    /// Restore an edge tombstone owned by the caller's primary namespace.
    pub async fn restore_edge(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<Option<(Edge, bool)>> {
        let Some(edge) = self.get_edge_including_deleted(token, id).await? else {
            return Ok(None);
        };
        if edge.namespace != token.namespace().as_str() {
            return Ok(None);
        }
        if edge.deleted_at.is_none() {
            return Ok(Some((edge, false)));
        }
        let updated_at = Utc::now();
        let plan = AtomicOpPlan::Update(Box::new(UpdatePlan {
            graph_effects: Vec::new(),
            target_id: id,
            statements: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "UPDATE graph_edges SET deleted_at=NULL, updated_at=?1 \
                          WHERE id=?2 AND namespace=?3 AND deleted_at IS NOT NULL"
                        .into(),
                    params: vec![
                        SqlValue::Integer(updated_at.timestamp_micros()),
                        SqlValue::Text(id.to_string()),
                        SqlValue::Text(edge.namespace.clone()),
                    ],
                    label: Some("edge-restore".into()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
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
                let mut restored = edge;
                restored.deleted_at = None;
                restored.updated_at = updated_at;
                Ok(Some((restored, true)))
            }
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("edge restore rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }

    /// Soft-delete or hard-delete a note by ID.
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) and cleans up FTS and vector indexes, preventing dangling
    /// references for `annotates` edges that target this note.
    /// Soft delete also cleans FTS and vector indexes; edges are left in place.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    /// Cascade and index cleanup target the RECORD's stored namespace, not the caller token's.
    /// Returns `Ok(false)` if the note does not exist.
    pub async fn delete_note(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let (deleted, degradations) = self
            .delete_note_with_post_commit_report(token, id, hard)
            .await?;
        legacy_post_commit_result("delete_note", id, deleted, degradations)
    }

    /// Delete the note and return diagnostics for any failed work after the
    /// row change committed. A degradation is non-retryable: callers must not
    /// repeat a create or delete because an index or telemetry leg failed.
    pub async fn delete_note_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<(bool, Vec<PostCommitDegradation>)> {
        let note_store = self.notes(token)?;
        let note = if hard {
            match note_store.get_note_including_deleted(id).await? {
                Some(n) => n,
                None => return Ok((false, Vec::new())),
            }
        } else {
            match note_store.get_note(id).await? {
                Some(n) => n,
                None => return Ok((false, Vec::new())),
            }
        };
        if let Some(error) = self.stream_member_error(&note).await? {
            return Err(error);
        }
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Route index cleanup through the RECORD's namespace, not the caller's.
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&note.namespace)
                .map_err(|e| RuntimeError::Internal(format!("note namespace invalid: {e}")))?,
        );
        let record_ns = note.namespace.clone();
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        // On hard delete, the row delete and the incident-edge cascade (including
        // already-soft-deleted edges) run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`. Index cleanup follows the
        // commit; it is best-effort and idempotent, unlike the row/edge pair.
        let deleted = if hard {
            self.atomic_hard_delete_with_edge_purge(
                note_hard_delete_statement(id),
                id,
                &record_ns,
                &actor,
                SubstrateKind::Note,
            )
            .await?
        } else {
            note_store.delete_note(id, mode).await?
        };
        let mut degradations = Vec::new();
        if deleted {
            let fts_result = match self.text_for_notes(&record_tok) {
                Ok(store) => store
                    .delete_document(&record_ns, id)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = fts_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_note",
                    id,
                    "fts_cleanup",
                    error,
                );
            }
            // Try every model even when FTS or another model failed.
            for model_name in self.registered_embedding_model_names() {
                let vector_result = match self.vectors_for_model(&record_tok, &model_name) {
                    Ok(store) => store.delete(id).await.map_err(RuntimeError::from),
                    Err(error) => Err(error),
                };
                if let Err(error) = vector_result {
                    record_post_commit_degradation(
                        &mut degradations,
                        "delete_note",
                        id,
                        "vector_cleanup",
                        format!("{model_name}: {error}"),
                    );
                }
            }
            let event = khive_storage::event::Event::new(
                record_ns.clone(),
                "delete",
                EventKind::NoteDeleted,
                SubstrateKind::Note,
                "",
            )
            .with_target(id)
            .with_payload(serde_json::json!({"id": id, "namespace": record_ns, "hard": hard}));
            let event_result = match self.events(&record_tok) {
                Ok(store) => store.append_event(event).await.map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = event_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_note",
                    id,
                    "event_append",
                    error,
                );
            }
            // A soft OR hard delete removes the note's vectors/FTS document
            // above: any pack-owned vector-derived cache (e.g.
            // khive-pack-memory's warm ANN index) needs to know the corpus
            // changed, reached via this generic hook so khive-runtime never
            // takes a dependency on khive-pack-memory. No-op when no pack has
            // installed a hook.
            self.fire_note_mutation_hook(&note.kind, id).await;
        }
        Ok((deleted, degradations))
    }

    /// Row-first compensating delete for rolling back a partially-written note
    /// (e.g. `dual_write_message` rollback after a later delivery step fails).
    /// Unlike [`KhiveRuntime::delete_note`], which cleans up graph/FTS/
    /// vector indexes *before* removing the row, this removes the row first so
    /// that a cleanup failure afterward cannot leave the compensated note live.
    ///
    /// Returns `Ok(())` once the row is gone (whether or not cleanup fully
    /// succeeded). Returns `Err(RuntimeError::Internal)` naming the failed
    /// cleanup legs when row removal succeeded but cleanup did not — the
    /// message is gone, but stale index entries may remain and should be
    /// surfaced to the caller rather than silently discarded.
    ///
    /// Returns `Ok(())` immediately, with no cleanup attempted, if the note
    /// does not exist (nothing to compensate).
    ///
    /// Not a general-purpose replacement for `delete_note(..., hard=true)`:
    /// normal hard delete still needs cleanup-first semantics (no dangling
    /// references) since a caller-visible error there should not remove the row.
    pub async fn delete_note_row_first_for_compensation(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<()> {
        let note_store = self.notes(token)?;
        let Some(note) = note_store.get_note_including_deleted(id).await? else {
            return Ok(());
        };
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&note.namespace)
                .map_err(|e| RuntimeError::Internal(format!("note namespace invalid: {e}")))?,
        );
        let record_ns = note.namespace.clone();

        // Critical ordering: remove the row before any cleanup that can fail.
        note_store.delete_note(id, DeleteMode::Hard).await?;

        #[cfg(any(test, feature = "fault-injection"))]
        {
            let armed = ROLLBACK_CLEANUP_FAIL_NS.lock().unwrap().take();
            if armed.as_deref() == Some(record_ns.as_str()) {
                return Err(RuntimeError::Internal(
                    "row removed but compensation cleanup failed: injected=true".to_string(),
                ));
            }
        }

        let mut cleanup_errors = Vec::new();
        if let Err(e) = self.graph(&record_tok)?.purge_incident_edges(id).await {
            cleanup_errors.push(format!("graph={e}"));
        }
        if let Err(e) = self
            .text_for_notes(&record_tok)?
            .delete_document(&record_ns, id)
            .await
        {
            cleanup_errors.push(format!("fts={e}"));
        }
        for model_name in self.registered_embedding_model_names() {
            if let Err(e) = self
                .vectors_for_model(&record_tok, &model_name)?
                .delete(id)
                .await
            {
                cleanup_errors.push(format!("vector[{model_name}]={e}"));
            }
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::Internal(format!(
                "row removed but compensation cleanup failed: {}",
                cleanup_errors.join("; ")
            )))
        }
    }
}

/// Result of a GQL/SPARQL query with optional validation warnings.
#[derive(Clone, Debug, Serialize)]
pub struct QueryResult {
    pub rows: Vec<SqlRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Zero-based offset of this deterministic result page.
    pub offset: usize,
    /// Effective payload bound after composing query `LIMIT` and server page size.
    pub page_size: usize,
    /// `true` when at least one additional match exists after this page.
    pub has_more: bool,
    /// GQL continuation offset. Present exactly when GQL has another page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Backward-compatible alias for `has_more` (#1168, #1247, #1601).
    pub truncated: bool,
}

/// Outcome of [`KhiveRuntime::update_edge_symmetric_dml`]'s in-transaction DML.
#[derive(Debug)]
enum SymmetricEdgeUpdateOutcome {
    /// A canonical row already existed (ADR-039 DO NOTHING): the
    /// requested edge was deleted, the existing canonical row (this id)
    /// left untouched.
    Absorbed(String),
    /// No conflict; the requested edge was updated in place.
    Updated,
    /// No conflict, but the in-place `UPDATE` matched zero rows because
    /// the row's revision or deletion marker moved after it was fetched —
    /// a concurrent writer raced this update and must be refused, not
    /// silently overwritten by a stale full-row write.
    Stale,
}

impl KhiveRuntime {
    // ---- Query operations ----

    /// Execute a GQL or SPARQL query string, returning raw SQL rows.
    ///
    /// The query is compiled to SQL with the namespace scope applied.
    /// GQL syntax: `MATCH (a:concept)-[e:extends]->(b) RETURN a, b LIMIT 10`
    /// SPARQL syntax: `SELECT ?a WHERE { ?a :kind "concept" . }`
    pub async fn query(&self, token: &NamespaceToken, query: &str) -> RuntimeResult<Vec<SqlRow>> {
        Ok(self
            .query_with_metadata(token, query, khive_query::CompileOptions::default())
            .await?
            .rows)
    }

    /// Execute a GQL/SPARQL query, returning rows and any validation warnings.
    pub async fn query_with_metadata(
        &self,
        token: &NamespaceToken,
        query: &str,
        mut opts: khive_query::CompileOptions,
    ) -> RuntimeResult<QueryResult> {
        use khive_query::QueryValue;
        use khive_storage::types::SqlValue;

        let (language, ast) = khive_query::language::parse_auto_with_language(query)?;
        if opts.max_limit == 0 {
            return Err(RuntimeError::InvalidInput(
                "query page size must be at least 1".into(),
            ));
        }
        let offset = ast.offset;
        let page_size = ast.limit.unwrap_or(opts.max_limit).min(opts.max_limit);
        opts.scopes = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_string())
            .collect();
        let compiled = khive_query::compile(&ast, &opts)?;
        let mut warnings = compiled.warnings;
        let truncation_check = compiled.truncation_check;

        warnings.extend(self.with_pack_edge_rules(|pack_rules| {
            static_impossible_edge_pattern_warnings(language, &ast.pattern, pack_rules)
        }));

        // Convert QueryValue params (query-layer type) to SqlValue (storage-layer type)
        // at the query–storage boundary.
        let params: Vec<SqlValue> = compiled
            .params
            .into_iter()
            .map(|qv| match qv {
                QueryValue::Null => SqlValue::Null,
                QueryValue::Integer(n) => SqlValue::Integer(n),
                QueryValue::Float(f) => SqlValue::Float(f),
                QueryValue::Text(s) => SqlValue::Text(s),
                QueryValue::Blob(b) => SqlValue::Blob(b),
            })
            .collect();

        let mut reader = self.sql().reader().await?;
        let stmt = SqlStatement {
            sql: compiled.sql,
            params,
            label: None,
        };
        let mut rows = reader.query_all(stmt).await?;

        // When the effective page size was the binding constraint, the compiled
        // SQL asked for one extra (sentinel) row. Its presence in the actual
        // result set — not the requested LIMIT — is the continuation signal.
        let mut truncated = false;
        if let Some(check) = truncation_check {
            if rows.len() > check.max_limit {
                rows.truncate(check.max_limit);
                truncated = true;
            }
        }

        let next_offset = if truncated && language == khive_query::QueryLanguage::Gql {
            let next = offset.checked_add(rows.len()).ok_or_else(|| {
                RuntimeError::InvalidInput("GQL next_offset exceeds usize::MAX".into())
            })?;
            if next == offset {
                return Err(RuntimeError::InvalidInput(
                    "query page did not advance; page size must be at least 1".into(),
                ));
            }
            i64::try_from(next).map_err(|_| {
                RuntimeError::InvalidInput("GQL next_offset exceeds i64::MAX".into())
            })?;
            Some(next)
        } else {
            None
        };

        if truncated {
            let Some(check) = truncation_check else {
                return Err(RuntimeError::Internal(
                    "truncated query result is missing sentinel metadata".into(),
                ));
            };
            let bound = match check.requested_limit {
                Some(requested) => {
                    format!("requested query LIMIT {requested} exceeds the effective page size")
                }
                None => "the query has no explicit LIMIT".to_string(),
            };
            let warning = match language {
                khive_query::QueryLanguage::Gql => {
                    let Some(next) = next_offset else {
                        return Err(RuntimeError::Internal(
                            "truncated GQL result is missing its continuation offset".into(),
                        ));
                    };
                    format!(
                        "result page capped at {} rows because {bound}; more matches exist. \
                         Continue the same GQL query with `SKIP {next}` (the machine-readable \
                         `next_offset`) and keep the same page size.",
                        check.max_limit
                    )
                }
                khive_query::QueryLanguage::Sparql => format!(
                    "result page capped at {} rows because {bound}; more matches exist. \
                     SPARQL OFFSET paging is not part of the supported dialect.",
                    check.max_limit
                ),
            };
            warnings.push(warning);
        }

        Ok(QueryResult {
            rows,
            warnings,
            offset,
            page_size,
            has_more: truncated,
            next_offset,
            truncated,
        })
    }

    /// Soft-delete or hard-delete an entity by ID (soft delete by default).
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) to prevent dangling references. Soft delete also cleans FTS
    /// and vector indexes; edges are left in place.
    /// Routed attachment cleanup is performed by the registry after ownership resolution.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn delete_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let (deleted, degradations) = self
            .delete_entity_with_post_commit_report(token, id, hard)
            .await?;
        legacy_post_commit_result("delete_entity", id, deleted, degradations)
    }

    /// The committed delete together with non-retryable index/telemetry errors.
    pub async fn delete_entity_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<(bool, Vec<PostCommitDegradation>)> {
        let entity = if hard {
            match self
                .entities(token)?
                .get_entity_including_deleted(id)
                .await?
            {
                Some(e) => e,
                None => return Ok((false, Vec::new())),
            }
        } else {
            match self.entities(token)?.get_entity(id).await? {
                Some(e) => e,
                None => return Ok((false, Vec::new())),
            }
        };
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Route cascade and index cleanup through the RECORD's namespace, not the caller's.
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&entity.namespace)
                .map_err(|e| RuntimeError::Internal(format!("entity namespace invalid: {e}")))?,
        );
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        // On hard delete, the row delete and the incident-edge cascade (including
        // already-soft-deleted edges) run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`. Index cleanup follows the
        // commit; it is best-effort and idempotent, unlike the row/edge pair.
        let deleted = if hard {
            // Cross-backend attachment cleanup requires the registry's ownership check.
            self.atomic_hard_delete_with_edge_purge(
                entity_hard_delete_statement(id),
                id,
                &entity.namespace,
                &actor,
                SubstrateKind::Entity,
            )
            .await?
        } else {
            self.entities(token)?.delete_entity(id, mode).await?
        };
        let mut degradations = Vec::new();
        if deleted {
            let ns = entity.namespace.clone();
            let fts_result = match self.text(&record_tok) {
                Ok(store) => store
                    .delete_document(&ns, id)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = fts_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_entity",
                    id,
                    "fts_cleanup",
                    error,
                );
            }
            for model_name in self.registered_embedding_model_names() {
                let vector_result = match self.vectors_for_model(&record_tok, &model_name) {
                    Ok(store) => store.delete(id).await.map_err(RuntimeError::from),
                    Err(error) => Err(error),
                };
                if let Err(error) = vector_result {
                    record_post_commit_degradation(
                        &mut degradations,
                        "delete_entity",
                        id,
                        "vector_cleanup",
                        format!("{model_name}: {error}"),
                    );
                }
            }
            let event = khive_storage::event::Event::new(
                ns.clone(),
                "delete",
                EventKind::EntityDeleted,
                SubstrateKind::Entity,
                "",
            )
            .with_target(id)
            .with_payload(serde_json::json!({"id": id, "namespace": ns, "hard": hard}));
            let event_result = match self.events(&record_tok) {
                Ok(store) => store.append_event(event).await.map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = event_result {
                record_post_commit_degradation(
                    &mut degradations,
                    "delete_entity",
                    id,
                    "event_append",
                    error,
                );
            }
        }
        Ok((deleted, degradations))
    }

    pub(crate) async fn delete_entity_attachments_on_core(&self, id: Uuid) -> RuntimeResult<bool> {
        let core = self.core();
        drop(core.attachments()?);
        let statement = khive_db::stores::attachment::delete_record_attachments_statement(
            id,
            AttachmentSubstrate::Entity,
        );
        Ok(core.sql().writer().await?.execute(statement).await? > 0)
    }

    /// Count entities in a namespace, optionally filtered.
    pub async fn count_entities(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let ns_strs: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let filter = EntityFilter {
            kinds: match kind {
                Some(k) => vec![k.to_string()],
                None => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        Ok(self
            .entities(token)?
            .count_entities(token.namespace().as_str(), filter)
            .await?)
    }

    /// Return the coupled entity total and optional type counts for `stats`.
    pub async fn entity_stats_counts(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<EntityStatsCounts> {
        entity_stats_counts(self.entities(token)?.as_ref(), token).await
    }

    // ---- Edge CRUD operations ----

    /// Fetch a single edge by id.
    ///
    /// UUID v4 is globally unique: returns the edge regardless of which
    /// namespace the token carries. `Ok(None)` means the edge does not exist at all.
    pub async fn get_edge(
        &self,
        _token: &NamespaceToken,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        let mut reader = self.sql().reader().await?;
        let record_ns = reader
            .query_scalar(SqlStatement {
                sql: "SELECT namespace FROM graph_edges \
                      WHERE id = ?1 AND deleted_at IS NULL LIMIT 1"
                    .into(),
                params: vec![SqlValue::Text(edge_id.to_string())],
                label: Some("get_edge_namespace".into()),
            })
            .await?;

        let Some(SqlValue::Text(record_ns)) = record_ns else {
            return Ok(None);
        };
        // Route the storage fetch through the record's own namespace — the token is
        // just the caller context; by-ID ops cross namespace boundaries.
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        Ok(self
            .graph(&record_tok)?
            .get_edge(LinkId::from(edge_id))
            .await?)
    }

    /// Read live edges by ID in input order, without a visibility predicate.
    ///
    /// Stored namespaces are validated before the corresponding edge decode.
    /// Each namespace group uses its own graph capability; missing rows remain
    /// `None`. Group failures belong to their first input, and the earliest
    /// input error wins after all groups in that bounded window are observed.
    /// Metadata statement failures are fatal batch errors. Windows are separate
    /// read observations, not a snapshot of the whole request.
    pub async fn get_edges_by_id(
        &self,
        _token: &NamespaceToken,
        ids: &[Uuid],
    ) -> RuntimeResult<Vec<Option<Edge>>> {
        let mut edges = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(900) {
            let window = self.prepare_edge_read_window(chunk).await?;
            edges.extend(
                Self::hydrate_edge_read_window(chunk, window, |record_token| {
                    self.graph(record_token)
                })
                .await?,
            );
        }
        Ok(edges)
    }

    async fn prepare_edge_read_window(&self, ids: &[Uuid]) -> RuntimeResult<EdgeReadWindow> {
        let placeholders = (1..=ids.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut reader = self.sql().reader().await?;
        let rows = reader
            .query_all(SqlStatement {
                sql: format!(
                    "SELECT id, namespace FROM graph_edges WHERE id IN ({placeholders}) AND deleted_at IS NULL"
                ),
                params: ids.iter().map(|id| SqlValue::Text(id.to_string())).collect(),
                label: Some("get_edge_namespace".into()),
            })
            .await?;
        let mut namespaces = HashMap::with_capacity(rows.len());
        for row in rows {
            let Some(SqlValue::Text(id)) = row.columns.first().map(|column| &column.value) else {
                return Err(RuntimeError::Internal(
                    "edge namespace lookup returned an invalid id".into(),
                ));
            };
            let id = Uuid::parse_str(id).map_err(|e| {
                RuntimeError::Internal(format!("edge namespace lookup returned an invalid id: {e}"))
            })?;
            let value = row
                .columns
                .get(1)
                .map(|column| column.value.clone())
                .unwrap_or(SqlValue::Null);
            namespaces.insert(id, value);
        }
        let mut window = EdgeReadWindow {
            outcomes: (0..ids.len()).map(|_| Some(Ok(None))).collect(),
            groups: Vec::new(),
        };
        let mut group_indices = HashMap::new();
        for (index, id) in ids.iter().enumerate() {
            let Some(SqlValue::Text(record_ns)) = namespaces.get(id) else {
                continue;
            };
            match khive_types::Namespace::parse(record_ns) {
                Ok(namespace) => {
                    let next_group = window.groups.len();
                    let group = *group_indices.entry(record_ns.clone()).or_insert(next_group);
                    if group == next_group {
                        window.groups.push((namespace, Vec::new()));
                    }
                    window.groups[group].1.push(index);
                    window.outcomes[index] = None;
                }
                Err(error) => {
                    window.outcomes[index] = Some(Err(RuntimeError::Internal(format!(
                        "edge namespace invalid: {error}"
                    ))));
                }
            }
        }
        Ok(window)
    }

    async fn hydrate_edge_read_window<F>(
        ids: &[Uuid],
        mut window: EdgeReadWindow,
        mut graph: F,
    ) -> RuntimeResult<Vec<Option<Edge>>>
    where
        F: FnMut(&NamespaceToken) -> RuntimeResult<std::sync::Arc<dyn khive_storage::GraphStore>>,
    {
        for (namespace, indices) in window.groups {
            let record_token = NamespaceToken::for_namespace(namespace);
            let group_ids: Vec<LinkId> = indices
                .iter()
                .map(|&index| LinkId::from(ids[index]))
                .collect();
            let outcomes = match graph(&record_token) {
                Ok(store) => store
                    .get_edge_read_outcomes(&group_ids)
                    .await
                    .map_err(RuntimeError::from),
                Err(error) => Err(error),
            };
            match outcomes {
                Ok(outcomes) if outcomes.len() == indices.len() => {
                    for (index, outcome) in indices.into_iter().zip(outcomes) {
                        window.outcomes[index] = Some(outcome.map_err(RuntimeError::from));
                    }
                }
                Ok(_) => {
                    window.outcomes[indices[0]] = Some(Err(RuntimeError::Internal(
                        "edge batch returned an invalid outcome count".into(),
                    )));
                }
                Err(error) => {
                    window.outcomes[indices[0]] = Some(Err(error));
                }
            }
        }
        window
            .outcomes
            .into_iter()
            .map(|outcome| {
                outcome.unwrap_or_else(|| {
                    Err(RuntimeError::Internal(
                        "edge batch omitted an input outcome".into(),
                    ))
                })
            })
            .collect()
    }

    /// Fetch a single edge by id.
    ///
    /// Delegates to `get_edge`: no visible-set check.  By-ID ops are
    /// namespace-agnostic; UUID v4 is globally unique.
    pub async fn get_edge_visible(
        &self,
        token: &NamespaceToken,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        self.get_edge(token, edge_id).await
    }

    /// Fetch an edge by UUID including soft-deleted rows.
    ///
    /// Returns the edge regardless of which namespace the token carries:
    /// UUID v4 is globally unique. Used by the hard-delete path so that a
    /// soft-deleted edge can still be purged via its edge ID.
    pub async fn get_edge_including_deleted(
        &self,
        _token: &NamespaceToken,
        edge_id: Uuid,
    ) -> RuntimeResult<Option<Edge>> {
        let mut reader = self.sql().reader().await?;
        let record_ns = reader
            .query_scalar(SqlStatement {
                sql: "SELECT namespace FROM graph_edges WHERE id = ?1 LIMIT 1".into(),
                params: vec![SqlValue::Text(edge_id.to_string())],
                label: Some("get_edge_including_deleted_namespace".into()),
            })
            .await?;

        let Some(SqlValue::Text(record_ns)) = record_ns else {
            return Ok(None);
        };
        // Route through the record's own namespace store (no namespace equality check).
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        Ok(self
            .graph(&record_tok)?
            .get_edge_including_deleted(LinkId::from(edge_id))
            .await?)
    }

    /// Fetch an edge by natural key (namespace, canonical source/target, relation)
    /// including soft-deleted rows. Unlike [`Self::list_edges`]/[`Self::list_edges_after`],
    /// which always filter `deleted_at IS NULL`, this can render a tombstoned symmetric-edge
    /// survivor (ADR-039 DO NOTHING conflict absorption) — used by the atomic-apply
    /// post-commit result renderer, which otherwise reports "not found" for a committed
    /// update whose surviving row happens to be soft-deleted.
    ///
    /// `token` selects the store instance; `namespace` is the natural key's own
    /// namespace and is bound into the query explicitly. These can legitimately differ:
    /// the record namespace is fixed at prepare time (`EdgeNaturalKey::namespace`) and by-ID
    /// edge updates are namespace-agnostic (ADR-007 Rev 6), so the caller's ambient `token`
    /// namespace is never a safe substitute for the record's own — the prior implicit
    /// `self.namespace` scoping is exactly the bug this parameter closes (khive#1213/#1214).
    pub async fn get_edge_by_natural_key_including_deleted(
        &self,
        token: &NamespaceToken,
        namespace: &str,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<Option<Edge>> {
        Ok(self
            .graph(token)?
            .get_edge_by_natural_key_including_deleted(namespace, source_id, target_id, relation)
            .await?)
    }

    /// Maximum rows returned by a single [`Self::list_edges`] /
    /// [`Self::list_edges_after`] page. A lower bound the docs promise callers
    /// can rely on; kept as a named constant so tests can exercise pagination
    /// (page tiling, out-of-range offsets) without needing >1000 real rows.
    pub const EDGE_LIST_MAX_LIMIT: u32 = 1000;

    /// List edges matching `filter`, paging by `offset`. `limit` is capped at
    /// [`Self::EDGE_LIST_MAX_LIMIT`]; defaults to 100. For O(1)-at-depth walks
    /// over large edge populations, prefer [`Self::list_edges_after`] instead
    /// of paging offset deep.
    pub async fn list_edges(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Edge>> {
        let limit = limit.min(Self::EDGE_LIST_MAX_LIMIT);
        let visible = token.visible_namespaces();

        // Common case: a single visible namespace — page directly against the
        // store so `offset`/`limit` reach SQL unmodified.
        if let [ns] = visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges(
                    filter.into(),
                    vec![SortOrder {
                        field: EdgeSortField::CreatedAt,
                        direction: khive_storage::types::SortDirection::Asc,
                    }],
                    PageRequest {
                        offset: offset.into(),
                        limit,
                    },
                )
                .await?;
            return Ok(page.items);
        }

        // Multi-namespace visibility: one deterministic query with
        // `namespace IN (...)` and real SQL paging, mirroring
        // `list_entities`. Fetching per-namespace prefixes and slicing a
        // client-side merge re-sorted by UUID floats the offset window
        // between calls — pages silently duplicate and skip rows, so
        // enumeration never covers the set (#2088).
        let ns_strs: Vec<String> = visible.iter().map(|ns| ns.as_str().to_owned()).collect();
        let sort = vec![SortOrder {
            field: EdgeSortField::CreatedAt,
            direction: khive_storage::types::SortDirection::Asc,
        }];
        let graph = self.graph(token)?;
        match graph
            .query_edges_in_namespaces(
                &ns_strs,
                filter.clone().into(),
                sort.clone(),
                PageRequest {
                    offset: offset.into(),
                    limit,
                },
            )
            .await
        {
            Ok(page) => Ok(page.items),
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "query_edges_in_namespaces" =>
            {
                // Backend exercises the trait default (no batched
                // namespace query support): fall back to one `query_edges`
                // call per namespace. Unlike the pre-image fix for #2088,
                // this fetches an `offset + limit` prefix from every
                // namespace and merges by the *same* `(created_at, id)` key
                // each per-namespace fetch already orders by — the
                // pre-image bug sorted the merged set by UUID alone, a key
                // unrelated to the order each per-namespace prefix was cut
                // at, which floated the offset window and silently
                // duplicated/skipped rows across pages. Sorting by the
                // fetch's own order key keeps the top `offset + limit` of
                // the merge exactly equal to the true global prefix.
                let fetch_limit = offset.saturating_add(limit);
                let mut namespace_prefixes = Vec::new();
                for ns in visible {
                    let temp = NamespaceToken::for_namespace(ns.clone());
                    let page = self
                        .graph(&temp)?
                        .query_edges(
                            filter.clone().into(),
                            sort.clone(),
                            PageRequest {
                                offset: 0,
                                limit: fetch_limit,
                            },
                        )
                        .await?;
                    namespace_prefixes.push(page.items);
                }
                Ok(Self::merge_paged_namespace_edges(
                    namespace_prefixes,
                    offset,
                    limit,
                ))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Merge per-namespace `(created_at, id)`-ordered edge prefixes (each
    /// already fetched up to `offset + limit` from its own namespace, as
    /// [`Self::list_edges`]'s trait-default fallback does) into one global
    /// `[offset, offset + limit)` page.
    ///
    /// Sorting by the *same key each prefix was already cut at* is what
    /// keeps this exact: the top `offset + limit` of the merged set is then
    /// provably equal to the true global prefix (a standard k-way merge
    /// argument — no element beyond position `offset + limit` in any single
    /// namespace can appear before that position in the global order). The
    /// pre-image #2088 bug instead re-sorted the merged set by UUID alone —
    /// a key unrelated to the order each namespace's prefix was fetched in —
    /// which floated the offset window and silently duplicated/skipped rows
    /// across pages.
    fn merge_paged_namespace_edges(
        namespace_prefixes: Vec<Vec<Edge>>,
        offset: u32,
        limit: u32,
    ) -> Vec<Edge> {
        let mut results: Vec<Edge> = namespace_prefixes.into_iter().flatten().collect();
        results.sort_by_key(|e| (e.created_at, Uuid::from(e.id)));
        let start = (offset as usize).min(results.len());
        let end = (start + limit as usize).min(results.len());
        results[start..end].to_vec()
    }

    /// Keyset (seek) page of edges matching `filter`, ordered by immutable
    /// database-assigned insertion sequence. `after` is the last edge id from the
    /// previous page (exclusive); omit to start from the beginning. Returns
    /// `(items, next_after)` — `next_after` is `Some` when more rows remain
    /// past this page.
    ///
    /// Unlike [`Self::list_edges`], this is O(log n + limit) at any depth and
    /// genuinely new inserts are appended after already-issued boundaries. The
    /// cursor row is resolved including tombstones; a hard-deleted or
    /// out-of-scope cursor fails explicitly rather than hiding an incomplete
    /// traversal.
    pub async fn list_edges_after(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
        after: Option<Uuid>,
        limit: u32,
    ) -> RuntimeResult<(Vec<Edge>, Option<Uuid>)> {
        let limit = limit.clamp(1, Self::EDGE_LIST_MAX_LIMIT);
        let visible = token.visible_namespaces();
        let limit_usize = limit as usize;
        let cursor_store = self.graph(token)?;
        let after = match after {
            Some(id) => {
                let edge = self
                    .get_edge_including_deleted(token, id)
                    .await?
                    .ok_or_else(|| RuntimeError::NotFound(format!("edge cursor {id}")))?;
                Self::ensure_namespace_visible(&edge.namespace, token)?;
                let sequence = cursor_store.edge_sequence(id).await?.ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "edge cursor {id} has no insertion-sequence ledger row"
                    ))
                })?;
                Some(SeekCursor { sequence, id })
            }
            None => None,
        };

        if let [ns] = visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges_sequence_after(filter.into(), after, limit)
                .await?;
            return Ok((page.items, page.next_after.map(|cursor| cursor.id)));
        }

        // Multi-namespace visibility: seek each namespace from the same
        // immutable boundary, merge in global insertion order, then take the head of
        // the merged set as this page.
        let probe_limit = limit.saturating_add(1);
        let mut results = Vec::new();
        for ns in visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges_sequence_after(filter.clone().into(), after, probe_limit)
                .await?;
            results.extend(page.items);
        }
        let ids = results
            .iter()
            .map(|edge| Uuid::from(edge.id))
            .collect::<Vec<_>>();
        let sequences = cursor_store
            .edge_sequences(&ids)
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>();
        if let Some(missing) = ids.iter().find(|id| !sequences.contains_key(id)) {
            return Err(RuntimeError::Internal(format!(
                "edge {missing} has no insertion-sequence ledger row"
            )));
        }
        results.sort_by_key(|edge| {
            let id = Uuid::from(edge.id);
            (sequences[&id], id)
        });
        results.dedup_by_key(|e| Uuid::from(e.id));
        let has_more = results.len() > limit_usize;
        if has_more {
            results.truncate(limit_usize);
        }
        let next_after = if has_more {
            results.last().map(|e| Uuid::from(e.id))
        } else {
            None
        };
        Ok((results, next_after))
    }

    /// Count edges by relation, ignoring soft-deleted rows. Used by
    /// `stats()` to report the true per-relation population so full-graph
    /// audits know what they're sampling from before they walk it.
    pub async fn count_edges_by_relation(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<std::collections::HashMap<String, u64>> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        let counts = match graph
            .count_edges_by_relation_in_namespaces(&namespaces)
            .await
        {
            Ok(counts) => counts,
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_by_relation_in_namespaces" =>
            {
                let mut totals = HashMap::new();
                for namespace in token.visible_namespaces() {
                    let scoped = NamespaceToken::for_namespace(namespace.clone());
                    for (relation, count) in self.graph(&scoped)?.count_edges_by_relation().await? {
                        *totals.entry(relation).or_insert(0) += count;
                    }
                }
                return Ok(totals
                    .into_iter()
                    .map(|(relation, count)| (relation.to_string(), count))
                    .collect());
            }
            Err(error) => return Err(error.into()),
        };
        Ok(counts
            .into_iter()
            .map(|(relation, count)| (relation.to_string(), count))
            .collect())
    }

    /// Count edges by the base each endpoint resolves against. Used by
    /// `stats()` so a caller can name the denominator of a density figure
    /// instead of inheriting the flat edge total, which on a real store is
    /// mostly provenance.
    ///
    /// The per-namespace fallback sums the same buckets, so the aggregate and
    /// the fallback are checkable against each other and against
    /// `count_edges` by the invariant that the buckets sum to the total.
    pub async fn count_edges_by_endpoint_base(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<khive_storage::types::EdgeEndpointBaseCounts> {
        use khive_storage::types::EdgeEndpointBaseCounts;

        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        match graph
            .count_edges_by_endpoint_base_in_namespaces(&namespaces)
            .await
        {
            Ok(counts) => Ok(counts),
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_by_endpoint_base_in_namespaces"
                    || operation == "count_edges_by_endpoint_base" =>
            {
                let mut totals = EdgeEndpointBaseCounts::default();
                for namespace in token.visible_namespaces() {
                    let scoped = NamespaceToken::for_namespace(namespace.clone());
                    let counts = self.graph(&scoped)?.count_edges_by_endpoint_base().await?;
                    totals.entity_entity =
                        totals.entity_entity.saturating_add(counts.entity_entity);
                    totals.entity_note = totals.entity_note.saturating_add(counts.entity_note);
                    totals.note_entity = totals.note_entity.saturating_add(counts.note_entity);
                    totals.note_note = totals.note_note.saturating_add(counts.note_note);
                    totals.unresolved = totals.unresolved.saturating_add(counts.unresolved);
                }
                Ok(totals)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// DML-only body of the symmetric-relation conflict-resolution path in
    /// [`Self::update_edge`]. Runs the conflict-check SELECT, then either the
    /// DELETE+UPDATE (case b, a canonical row already exists) or the
    /// in-place UPDATE (case a, no conflict). Callers own the surrounding transaction
    /// boundary — this function issues DML only, no `BEGIN`/`COMMIT`/`ROLLBACK`.
    ///
    /// The in-place update is guarded on the fetched snapshot's revision and
    /// deletion marker (mirrors the non-symmetric `replace_edge_if_unchanged`
    /// guard) and requires the replacement revision to strictly advance.
    /// Shares its DML text with the atomic `prepare_update_edge` symmetric
    /// branch — see docs/operations.md#update_edge_symmetric_dml.
    #[allow(clippy::too_many_arguments)]
    fn update_edge_symmetric_dml(
        conn: &rusqlite::Connection,
        ns: &str,
        edge_id_str: &str,
        canon_src_str: &str,
        canon_tgt_str: &str,
        relation_str: &str,
        weight: f64,
        metadata: Option<String>,
        expected_updated_at_micros: i64,
        expected_deleted_at_micros: Option<i64>,
    ) -> Result<SymmetricEdgeUpdateOutcome, SqliteError> {
        // `updated_at` is stored in MICROSECONDS on `graph_edges` (every other
        // write path — `edge_upsert_statement`, `edge_soft_delete_statement` —
        // uses `timestamp_micros()`; the column is read back via
        // `micros_to_datetime`). `timestamp()` (seconds) here was a
        // pre-existing bug in this raw-SQL path, found while unifying it with
        // the atomic builder (which already used `timestamp_micros()`
        // correctly).
        //
        // The replacement revision must strictly advance past the snapshot
        // even when two operations land inside one clock microsecond;
        // saturating to i64::MAX would let the CAS accept a write without
        // advancing its revision, so that is not a valid fallback (mirrors
        // the note path).
        let minimum_updated_at_micros =
            expected_updated_at_micros.checked_add(1).ok_or_else(|| {
                SqliteError::InvalidData(format!(
                    "update_edge: edge {edge_id_str} updated_at is already at i64::MAX \
                         and cannot advance"
                ))
            })?;
        let now_ts = chrono::Utc::now()
            .timestamp_micros()
            .max(minimum_updated_at_micros);

        // Check for a conflicting canonical row (same namespace + natural key,
        // different id). This catches conflicts whether or not endpoints were flipped.
        let conflict_id: Option<String> = conn
            .query_row(
                khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                rusqlite::params![
                    &ns,
                    &canon_src_str,
                    &canon_tgt_str,
                    &relation_str,
                    &edge_id_str
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(SqliteError::Rusqlite)?;

        if let Some(existing_id) = conflict_id {
            // Case (b): canonical row already exists — ADR-039's edge-conflict
            // contract is ON CONFLICT DO NOTHING: drop the non-canonical edge
            // and leave the existing canonical row untouched (live or
            // tombstoned). Refreshing it from the discarded edge's
            // weight/target_backend/metadata and forcing deleted_at = NULL
            // would silently overwrite the survivor and resurrect a
            // tombstone — the same defect already fixed on the merge-rewire
            // path (`merge_entity_sql`/`merge_note_sql`); this path binds the
            // same shared `EDGE_SYMMETRIC_*_SQL` text and must honor the same
            // contract. Return the surviving id unchanged so the caller
            // re-fetches its real (unmodified) attributes.
            //
            // Guarded on the fetched snapshot's revision and deletion marker:
            // a concurrent writer that changed this edge between fetch and
            // this write must be refused, not silently deleted just because
            // a canonical survivor happens to exist. Zero affected rows here
            // means stale, not "no conflict" — the probe above already
            // confirmed a conflicting canonical row exists.
            let affected = conn
                .execute(
                    khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_GUARDED_SQL,
                    rusqlite::params![
                        &ns,
                        &edge_id_str,
                        expected_updated_at_micros,
                        expected_deleted_at_micros,
                    ],
                )
                .map_err(SqliteError::Rusqlite)?;
            if affected == 0 {
                return Ok(SymmetricEdgeUpdateOutcome::Stale);
            }
            Ok(SymmetricEdgeUpdateOutcome::Absorbed(existing_id))
        } else {
            // Case (a): no conflict — update source_id/target_id in-place,
            // preserving the original edge UUID. Guarded on the fetched
            // snapshot's revision and deletion marker: a concurrent writer
            // that moved this edge between fetch and this write must be
            // refused, not silently overwritten by a stale full-row update.
            let affected = conn
                .execute(
                    khive_db::stores::graph::EDGE_SYMMETRIC_UPDATE_INPLACE_SQL,
                    rusqlite::params![
                        &canon_src_str,
                        &canon_tgt_str,
                        &relation_str,
                        weight,
                        now_ts,
                        metadata,
                        &ns,
                        &edge_id_str,
                        expected_updated_at_micros,
                        expected_deleted_at_micros,
                    ],
                )
                .map_err(SqliteError::Rusqlite)?;
            if affected == 0 {
                return Ok(SymmetricEdgeUpdateOutcome::Stale);
            }
            Ok(SymmetricEdgeUpdateOutcome::Updated)
        }
    }

    /// Patch-style edge update. Only `Some(_)` fields are applied.
    ///
    /// When `relation` is `Some(new_rel)`, validates that the edge's existing endpoints
    /// are legal for `new_rel` before persisting. Weight-only updates (`relation = None`)
    /// skip validation. Returns `InvalidInput` if the new relation would violate the
    /// three-case endpoint contract; the edge is NOT mutated on error.
    ///
    /// For symmetric relations (`competes_with`, `composed_with`), endpoint order is
    /// canonicalised to `source_uuid < target_uuid` after validation. If a canonical
    /// row already exists at the target triple, the non-canonical edge is deleted and
    /// the existing canonical row is preserved unchanged (ADR-039 ON CONFLICT DO
    /// NOTHING, mirroring `merge_entity_sql`) — its attributes, including a soft-deleted
    /// `deleted_at`, are never overwritten by the discarded edge's patch.
    pub async fn update_edge(
        &self,
        token: &NamespaceToken,
        edge_id: Uuid,
        patch: crate::curation::EdgePatch,
    ) -> RuntimeResult<Edge> {
        // Fetch the edge by UUID: ID-only, no namespace check.
        // get_edge already uses the record's stored namespace internally.
        let graph_for_fetch = self.graph(token)?;
        let mut edge = graph_for_fetch
            .get_edge(LinkId::from(edge_id))
            .await?
            .ok_or_else(|| crate::RuntimeError::NotFound(format!("edge {edge_id}")))?;
        let expected_updated_at = edge.updated_at;
        let expected_deleted_at = edge.deleted_at;
        #[cfg(test)]
        crate::curation::race_seam::pause_after_read().await;

        // After fetching, all mutations and validation must use the
        // RECORD's namespace, not the caller's.  Derive record_tok from the stored edge
        // namespace so that endpoint validation, raw-SQL predicates, and graph routing
        // all address the correct backend partition.
        let record_ns: String = edge.namespace.clone();
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        let graph = self.graph(&record_tok)?;

        let mut changed_fields: Vec<&'static str> = Vec::new();
        if let Some(r) = patch.relation {
            // Validate before mutating — use the existing endpoints with the new relation.
            // Use record_tok so that endpoint existence checks look in the edge's own namespace.
            self.validate_edge_relation_endpoints(&record_tok, edge.source_id, edge.target_id, r)
                .await?;
            edge.relation = r;
            changed_fields.push("relation");
        }
        if let Some(w) = patch.weight {
            // Reject non-finite or out-of-range weight explicitly; do not silently
            // clamp invalid caller input (coding-standards §608-622).
            if !w.is_finite() || !(0.0..=1.0).contains(&w) {
                return Err(RuntimeError::InvalidInput(format!(
                    "edge weight must be a finite value in [0.0, 1.0]; got {w}"
                )));
            }
            edge.weight = w;
            changed_fields.push("weight");
        }
        if let Some(props) = patch.properties {
            crate::secret_gate::reject_reserved_secret_gate_property(Some(&props))?;
            edge.metadata = Some(props);
        }

        // For symmetric relations, canonicalise endpoint order and check
        // for natural-key conflicts regardless of whether endpoints were flipped.
        //
        // The raw-SQL path is used for ALL symmetric relations because `upsert_edge`
        // resolves ON CONFLICT(namespace,id) first and cannot detect a duplicate at
        // the natural key (namespace, source_id, target_id, relation) with a different
        // id. Bug-fix: this path must also run when endpoints are already canonical
        // (endpoints_flipped=false) to catch conflicts arising from a relation change
        // that collides with an existing canonical row.
        let (canon_src, canon_tgt) =
            canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);

        if edge.relation.is_symmetric() {
            // Raw-SQL path (mirrors merge_entity_sql).
            // Use record_ns (the stored edge namespace) — NOT token.namespace() — so that
            // WHERE namespace = ?N predicates match the actual row.
            let ns = record_ns.clone();
            let edge_id_str = edge_id.to_string();
            let relation_str = edge.relation.to_string();
            let canon_src_str = canon_src.to_string();
            let canon_tgt_str = canon_tgt.to_string();
            let weight = edge.weight;
            let metadata = edge
                .metadata
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());

            let expected_updated_at_micros = expected_updated_at.timestamp_micros();
            let expected_deleted_at_micros = expected_deleted_at.map(|v| v.timestamp_micros());

            let pool = self.backend().pool_arc();
            let writer_task = pool
                .writer_task_for_runtime_write(RuntimeWriteOperation::UpdateSymmetricEdge)
                .map_err(RuntimeError::Storage)?;

            let outcome: SymmetricEdgeUpdateOutcome = if let Some(writer_task) = writer_task {
                writer_task
                    .send(move |conn| {
                        Self::update_edge_symmetric_dml(
                            conn,
                            &ns,
                            &edge_id_str,
                            &canon_src_str,
                            &canon_tgt_str,
                            &relation_str,
                            weight,
                            metadata,
                            expected_updated_at_micros,
                            expected_deleted_at_micros,
                        )
                        .map_err(|e| {
                            khive_storage::StorageError::driver(
                                khive_storage::StorageCapability::Graph,
                                "update_edge",
                                e,
                            )
                        })
                    })
                    .await
                    .map_err(RuntimeError::Storage)?
            } else {
                tokio::task::spawn_blocking(move || {
                    let guard = pool.writer()?;
                    guard.transaction(|conn| {
                        Self::update_edge_symmetric_dml(
                            conn,
                            &ns,
                            &edge_id_str,
                            &canon_src_str,
                            &canon_tgt_str,
                            &relation_str,
                            weight,
                            metadata,
                            expected_updated_at_micros,
                            expected_deleted_at_micros,
                        )
                    })
                })
                .await
                .map_err(|e| {
                    RuntimeError::Internal(format!("update_edge: spawn_blocking join: {e}"))
                })?
                .map_err(RuntimeError::Sqlite)?
            };

            match outcome {
                SymmetricEdgeUpdateOutcome::Absorbed(sid) => {
                    // A conflict was absorbed (ADR-039 DO NOTHING): re-fetch the surviving
                    // canonical row so the caller receives its real, UNMODIFIED attributes —
                    // including soft-deleted rows, since the survivor's tombstone state (if
                    // any) must not be resurrected by the absorbed update either. Use
                    // record_tok — the surviving row lives in the same namespace as the
                    // original.
                    let surviving_uuid = Uuid::parse_str(&sid).map_err(|e| {
                        RuntimeError::Internal(format!(
                            "update_edge: surviving id parse failed: {e}"
                        ))
                    })?;
                    edge = self
                        .get_edge_including_deleted(&record_tok, surviving_uuid)
                        .await?
                        .ok_or_else(|| {
                            RuntimeError::Internal(format!(
                                "update_edge: surviving canonical row {surviving_uuid} vanished after update"
                            ))
                        })?;
                }
                SymmetricEdgeUpdateOutcome::Updated => {
                    // Reflect canonical endpoints in the returned edge (no conflict absorbed).
                    edge.source_id = canon_src;
                    edge.target_id = canon_tgt;
                }
                SymmetricEdgeUpdateOutcome::Stale => {
                    return Err(crate::curation::stale_edge_snapshot_error(edge_id));
                }
            }
        } else {
            // Non-symmetric: replace_edge_if_unchanged takes namespace from edge.namespace
            // (not from the graph store's routing namespace), so this is already
            // record-namespace correct. `graph` is already self.graph(&record_tok)?.
            // Guarded on the fetched snapshot's revision — a concurrent writer that moved
            // this edge between the fetch above and this write must be refused, not
            // silently overwritten by a full-row replacement derived from stale state.
            // `updated_at` must advance past the snapshot: the guard requires the
            // replacement revision to be strictly greater than the persisted one.
            // Make it strictly advance even when two operations land inside one
            // clock microsecond; saturating to i64::MAX would let the CAS accept
            // a write without advancing its revision, so that is not a valid
            // fallback (mirrors the note path).
            let minimum_updated_at_micros = expected_updated_at
                .timestamp_micros()
                .checked_add(1)
                .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "edge {edge_id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?;
            let now_micros = chrono::Utc::now()
                .timestamp_micros()
                .max(minimum_updated_at_micros);
            edge.updated_at =
                chrono::DateTime::from_timestamp_micros(now_micros).ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "edge {edge_id}: computed updated_at {now_micros} is not a valid timestamp"
                    ))
                })?;
            let persisted = graph
                .replace_edge_if_unchanged(edge.clone(), expected_updated_at, expected_deleted_at)
                .await?;
            if !persisted {
                return Err(crate::curation::stale_edge_snapshot_error(edge_id));
            }
        }

        // Audit event: use the record's namespace (record_ns) for the event payload.
        let event_store = self.events(&record_tok)?;
        let event = khive_storage::event::Event::new(
            record_ns.clone(),
            "update",
            EventKind::EdgeUpdated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(edge_id)
        .with_payload(
            serde_json::json!({"id": edge_id, "namespace": record_ns, "changed_fields": changed_fields}),
        );
        event_store.append_event(event).await.map_err(|e| {
            RuntimeError::Internal(format!("update_edge: event store write failed: {e}"))
        })?;

        Ok(edge)
    }

    /// Hard-delete an edge by id.
    ///
    /// Cascades to remove any `annotates` edges whose target is the deleted edge
    /// (`annotates` is note → anything; deleting an edge target leaves annotation
    /// edges dangling if not cleaned up). Returns `true` if the primary
    /// edge was removed.
    ///
    /// If `edge_id` does not refer to an edge (e.g. the caller passes an entity or
    /// note UUID by mistake), this method returns `Ok(false)` immediately with no
    /// side effects — it does **not** cascade inbound edges of the non-edge record.
    pub async fn delete_edge(
        &self,
        token: &NamespaceToken,
        edge_id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Fetch the edge first to obtain the record's own namespace.
        // By-ID ops cross namespace boundaries; all graph routing and audit
        // events must use the record namespace, not the caller's (mirrors update_edge).
        // For hard delete we also check soft-deleted rows so a soft-deleted edge
        // can still be purged via its edge ID.
        let edge = if hard {
            self.get_edge_including_deleted(token, edge_id).await?
        } else {
            self.get_edge(token, edge_id).await?
        };
        let Some(edge) = edge else {
            return Ok(false);
        };

        // Derive record_ns / record_tok from the fetched edge (mirrors update_edge).
        let record_ns: String = edge.namespace.clone();
        let record_tok = token.with_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        let graph = self.graph(&record_tok)?;
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);

        // Cascade: on hard delete, remove ALL annotates edges targeting this edge — including
        // already-soft-deleted ones: to prevent dangling graph_edges rows. The row
        // delete and the cascade purge run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`.
        // On soft delete the cascade is skipped (data-vs-view principle: soft-deleting the base
        // edge does not cascade to annotation edges; only a hard purge cleans up incident rows).
        let deleted = if hard {
            self.atomic_hard_delete_with_edge_purge(
                edge_hard_delete_statement(edge_id),
                edge_id,
                &record_ns,
                &actor,
                SubstrateKind::Entity,
            )
            .await?
        } else {
            graph.delete_edge(LinkId::from(edge_id), mode).await?
        };
        if deleted {
            // Audit event: use the record's namespace (record_ns), not the caller's namespace.
            let event_store = self.events(&record_tok)?;
            let event = khive_storage::event::Event::new(
                record_ns.clone(),
                "delete",
                EventKind::EdgeDeleted,
                SubstrateKind::Entity,
                "",
            )
            .with_target(edge_id)
            .with_payload(serde_json::json!({"id": edge_id, "namespace": record_ns, "hard": hard}));
            event_store.append_event(event).await.map_err(|e| {
                RuntimeError::Internal(format!("delete_edge: event store write failed: {e}"))
            })?;
        }
        Ok(deleted)
    }

    /// Count edges matching `filter` across the caller's visible namespaces.
    pub async fn count_edges(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
    ) -> RuntimeResult<u64> {
        let namespaces: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|namespace| namespace.as_str().to_owned())
            .collect();
        let graph = self.graph(token)?;
        match graph
            .count_edges_in_namespaces(&namespaces, filter.clone().into())
            .await
        {
            Ok(count) => Ok(count),
            Err(khive_storage::StorageError::Unsupported { operation, .. })
                if operation == "count_edges_in_namespaces" =>
            {
                let mut total = 0;
                for namespace in token.visible_namespaces() {
                    let scoped = NamespaceToken::for_namespace(namespace.clone());
                    total += self
                        .graph(&scoped)?
                        .count_edges(filter.clone().into())
                        .await?;
                }
                Ok(total)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Validate and construct an edge from a [`LinkSpec`] without writing to storage.
    ///
    /// Applies the full edge contract (endpoint validation, symmetric
    /// canonicalization, `dependency_kind` inference and metadata validation).
    /// Returns the constructed `Edge` on success; the caller is responsible for
    /// persisting it (e.g. via `upsert_edge` or `link_many`).
    ///
    /// The `token` must be a pre-authorized namespace token from the dispatch
    /// layer. If `spec.namespace` is set it must match `token.namespace()`;
    /// a mismatch returns `RuntimeError::InvalidInput`.
    pub async fn build_edge(&self, token: &NamespaceToken, spec: &LinkSpec) -> RuntimeResult<Edge> {
        self.build_edge_with_endpoint_kinds(token, spec)
            .await
            .map(|(edge, _)| edge)
    }

    async fn build_edge_with_endpoint_kinds(
        &self,
        token: &NamespaceToken,
        spec: &LinkSpec,
    ) -> RuntimeResult<(Edge, (EdgeEndpointKind, EdgeEndpointKind))> {
        validate_edge_metadata(spec.relation, spec.metadata.as_ref())?;
        let ns_str = match &spec.namespace {
            Some(s) => {
                let spec_ns = crate::Namespace::parse(s)
                    .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace: {e}")))?;
                if &spec_ns != token.namespace() {
                    return Err(RuntimeError::InvalidInput(
                        "LinkSpec namespace does not match token namespace".into(),
                    ));
                }
                s.as_str()
            }
            None => token.namespace().as_str(),
        };
        let endpoint_kinds = self
            .validate_edge_relation_endpoints(token, spec.source_id, spec.target_id, spec.relation)
            .await?;
        let (source_id, target_id) =
            canonical_edge_endpoints(spec.relation, spec.source_id, spec.target_id);
        let endpoint_kinds = canonical_edge_endpoint_kinds(
            spec.source_id,
            source_id,
            endpoint_kinds.0,
            endpoint_kinds.1,
        );
        let metadata = if spec.relation == EdgeRelation::DependsOn {
            // By-ID, unfiltered — matches the namespace-agnostic endpoint validation
            // above. The visible-set-scoped `resolve` would silently drop the
            // dependency_kind inference for endpoints validation now allows outside
            // the caller's visible set.
            match (
                self.resolve_edge_endpoint(token, source_id).await?,
                self.resolve_edge_endpoint(token, target_id).await?,
            ) {
                (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                    merge_dependency_kind(&src_e.kind, &tgt_e.kind, spec.metadata.clone())
                }
                _ => spec.metadata.clone(),
            }
        } else {
            spec.metadata.clone()
        };
        validate_edge_metadata(spec.relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        Ok((
            Edge {
                id: LinkId::from(Uuid::new_v4()),
                namespace: ns_str.to_string(),
                source_id,
                target_id,
                relation: spec.relation,
                weight: spec.weight,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata,
                target_backend: None,
            },
            endpoint_kinds,
        ))
    }

    /// Validate and atomically upsert a batch of edges.
    ///
    /// All edges are validated and constructed with `build_edge` before any
    /// write. If validation fails for any entry the entire batch is rejected
    /// (no writes occur). On success, all edges are persisted in a single
    /// source transaction with every lifecycle event and observation projection.
    ///
    /// After the bulk upsert, each edge is read back by its natural key
    /// (namespace, source_id, target_id, relation) so that the returned IDs
    /// are always the persisted row IDs, not the locally-generated UUIDs that
    /// may have been displaced by an ON CONFLICT DO UPDATE. This mirrors the
    /// same read-back applied to singleton `link()` and prevents phantom-ID
    /// exposure when callers upsert overlapping triples with `verbose=true`.
    ///
    /// All specs must share the same namespace; the namespace is taken from
    /// `token` (or validated against it if `spec.namespace` is set).
    pub async fn link_many(
        &self,
        token: &NamespaceToken,
        specs: Vec<LinkSpec>,
    ) -> RuntimeResult<Vec<Edge>> {
        self.link_many_observed(token, specs)
            .await
            .map(|rows| rows.into_iter().map(|row| row.edge).collect())
    }

    /// Observed all-or-nothing bulk link upsert. Every row carries its own
    /// create/update/resurrection disposition, and every tombstone policy is
    /// preflighted inside the same writer transaction before any mutation.
    pub async fn link_many_observed(
        &self,
        token: &NamespaceToken,
        specs: Vec<LinkSpec>,
    ) -> RuntimeResult<Vec<EdgeUpsertResult>> {
        self.link_many_guarded_observed(
            token,
            specs,
            GraphMutationPreconditions::default(),
            Vec::new(),
        )
        .await
        .map(|(rows, _)| rows)
    }

    /// Internal seam for khive-runtime; no compatibility promise.
    ///
    /// Guarded composition seam for packs.
    #[doc(hidden)]
    pub async fn link_many_guarded_observed(
        &self,
        token: &NamespaceToken,
        specs: Vec<LinkSpec>,
        preconditions: GraphMutationPreconditions,
        retirements: Vec<Edge>,
    ) -> RuntimeResult<(Vec<EdgeUpsertResult>, Vec<LinkId>)> {
        let namespace = token.namespace().as_str();
        let foreign_document = preconditions
            .document
            .as_ref()
            .is_some_and(|guard| guard.namespace != namespace);
        let foreign_edge = preconditions.edges.iter().any(|guard| {
            guard.namespace != namespace
                || guard
                    .expected
                    .as_ref()
                    .is_some_and(|edge| edge.namespace != namespace)
        });
        if foreign_document
            || foreign_edge
            || retirements.iter().any(|edge| edge.namespace != namespace)
        {
            return Err(RuntimeError::InvalidInput(
                "guarded link namespace does not match token namespace".into(),
            ));
        }
        if specs.is_empty()
            && preconditions.document.is_none()
            && preconditions.edges.is_empty()
            && retirements.is_empty()
        {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut edges = Vec::with_capacity(specs.len());
        let mut endpoint_kinds = Vec::with_capacity(specs.len());
        for spec in &specs {
            let (edge, kinds) = self.build_edge_with_endpoint_kinds(token, spec).await?;
            edges.push(edge);
            endpoint_kinds.push(kinds);
        }
        // `upsert_edges_guarded` re-checks every edge's endpoints as part of the
        // same write, not the separate per-spec `build_edge` validation reads
        // above. A concurrent hard-delete of any endpoint landing between those
        // reads and this write aborts the whole batch (all-or-nothing, no
        // partial write) instead of persisting a dangling edge. The failing
        // entry's index and its missing endpoint(s) come from the guard's own
        // in-transaction pre-check (`GuardedBatchOutcome::refused`), not a
        // post-hoc re-read of the batch after the write already failed.
        let requests = edges
            .into_iter()
            .zip(specs.iter())
            .map(|(edge, spec)| EdgeUpsertRequest {
                edge,
                resurrect: spec.resurrect,
            })
            .collect();
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::Batch {
                requests,
                guard_endpoints: true,
            },
            preconditions,
            retirements,
            move |outcome| {
                let GraphMutationOutcome::Batch(batch) = &outcome.mutation else {
                    return Err(Self::link_composition_shape_error(
                        "expected a written batch",
                    ));
                };
                if batch.rows.len() != endpoint_kinds.len() {
                    return Err(Self::link_composition_shape_error(
                        "edge result count differs from validated endpoint count",
                    ));
                }
                let mut events = Vec::with_capacity(batch.rows.len() + outcome.retired.len());
                for (row, (source_kind, target_kind)) in batch.rows.iter().zip(endpoint_kinds) {
                    events.push(Self::link_mutation_event(
                        &attribution,
                        row,
                        source_kind,
                        target_kind,
                    ));
                }
                for edge in &outcome.retired {
                    let edge_id = Uuid::from(edge.id);
                    events.push(
                        attribution.stamp(
                            Event::new(
                                edge.namespace.clone(),
                                "delete",
                                EventKind::EdgeDeleted,
                                SubstrateKind::Entity,
                                "",
                            )
                            .with_target(edge_id)
                            .with_payload(serde_json::json!({
                                "id": edge_id, "namespace": edge.namespace, "hard": false,
                            })),
                        ),
                    );
                }
                Ok(events)
            },
        )
        .await?;
        let retired = outcome.retired.into_iter().map(|edge| edge.id).collect();
        let GraphMutationOutcome::Batch(outcome) = outcome.mutation else {
            return Err(RuntimeError::Internal(
                "link_many: unexpected composition outcome".into(),
            ));
        };
        if let Some(refusal) = outcome.refusal {
            return match refusal.reason {
                EdgeUpsertRefusal::MissingEndpoints(missing) => {
                    Err(RuntimeError::GuardedWriteFailed(guarded_link_batch_failure(
                        &specs[refusal.entry_index],
                        refusal.entry_index,
                        missing,
                    )))
                }
                EdgeUpsertRefusal::ResurrectionRequired { edge } => {
                    Err(RuntimeError::InvalidInput(format!(
                        "batch entry {} targets soft-deleted edge {}; pass resurrect=true for that link",
                        refusal.entry_index, edge.id
                    )))
                }
            };
        }
        Ok((outcome.rows, retired))
    }

    /// Create a historical commit-to-project annotation without replacing a
    /// curated edge or reviving a tombstone. The store rechecks the exact live
    /// commit SHA and project under its writer transaction; only a newly
    /// inserted edge produces the ordinary LinkCreated lifecycle event.
    pub async fn link_commit_annotation_if_absent(
        &self,
        token: &NamespaceToken,
        commit_id: Uuid,
        project_id: Uuid,
        guard: CommitAnnotationGuard,
    ) -> RuntimeResult<CommitAnnotationInsertOutcome> {
        if !matches!(guard.expected_sha.len(), 40 | 64)
            || !guard
                .expected_sha
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(RuntimeError::InvalidInput(
                "expected full commit SHA".into(),
            ));
        }
        let edge = self
            .build_edge(
                token,
                &LinkSpec {
                    namespace: None,
                    source_id: commit_id,
                    target_id: project_id,
                    relation: EdgeRelation::Annotates,
                    weight: 1.0,
                    metadata: None,
                    resurrect: false,
                },
            )
            .await?;
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::CommitAnnotation { edge, guard },
            GraphMutationPreconditions::default(),
            Vec::new(),
            move |outcome| match &outcome.mutation {
                GraphMutationOutcome::CommitAnnotation(CommitAnnotationInsertOutcome::Created(
                    edge,
                )) => Ok(vec![Self::link_mutation_event(
                    &attribution,
                    &EdgeUpsertResult {
                        edge: edge.clone(),
                        disposition: EdgeUpsertDisposition::Created,
                        previous: None,
                    },
                    EdgeEndpointKind::Note,
                    EdgeEndpointKind::Entity,
                )]),
                _ => Err(Self::link_composition_shape_error(
                    "expected a created annotation",
                )),
            },
        )
        .await?;
        let GraphMutationOutcome::CommitAnnotation(result) = outcome.mutation else {
            return Err(RuntimeError::Internal(
                "link annotation: unexpected composition outcome".into(),
            ));
        };
        Ok(result)
    }

    /// Create a batch of entities atomically.
    ///
    /// All specs are validated before any write. If ANY spec fails validation
    /// (unknown kind, empty name, secret-gate violation), the method returns
    /// that error and no entities are written.
    ///
    /// Entity rows and their FTS documents are written in one SQLite transaction.
    /// Any statement failure rolls back the entire batch across both surfaces.
    /// Embedding is intentionally skipped: bulk structural ingest is the expected
    /// use-case, and dense vectors are backfilled later via a `reindex` call.
    pub async fn create_many(
        &self,
        token: &NamespaceToken,
        specs: Vec<EntityCreateSpec>,
    ) -> RuntimeResult<Vec<Entity>> {
        if specs.is_empty() {
            return Ok(vec![]);
        }
        let ns = token.namespace().as_str();

        // Phase 1: validate ALL specs before any write.
        // Includes entity-type validation via the pack-installed validator when available.
        // Any validation failure here guarantees zero rows are written.
        let mut entities = Vec::with_capacity(specs.len());
        for (index, spec) in specs.iter().enumerate() {
            entities.push(self.validate_bulk_entity(ns, spec, &format!("entity[{index}]"))?);
        }

        #[cfg(any(test, feature = "fault-injection"))]
        let fts_many_inject = consume_fault(&FTS_FAIL_MANY_NS, ns);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let fts_many_inject = false;

        #[cfg(any(test, feature = "fault-injection"))]
        let fts_many_inject_partial = consume_fault(&FTS_FAIL_MANY_PARTIAL_NS, ns);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let fts_many_inject_partial = false;

        let injected_failure_index = if fts_many_inject {
            Some(0)
        } else if fts_many_inject_partial {
            Some(usize::from(entities.len() > 1))
        } else {
            None
        };

        let _ = self.entities(token)?;
        let _ = self.text(token)?;

        let plans = entities
            .iter()
            .enumerate()
            .map(|(index, entity)| {
                let mut plan = bulk_entity_plan(entity)?;
                if injected_failure_index == Some(index) {
                    // Keep the guarded row insert; replace its FTS pair with the fault.
                    plan.statements.truncate(1);
                    plan.statements.push(PlanStatement {
                        statement: SqlStatement {
                            sql:
                                "INSERT INTO __khive_create_many_injected_failure__ DEFAULT VALUES"
                                    .to_string(),
                            params: vec![],
                            label: Some("fts-insert-injected-failure".to_string()),
                        },
                        guard: None,
                    });
                }
                Ok(AtomicOpPlan::AddEntity(plan))
            })
            .collect::<RuntimeResult<Vec<_>>>()?;

        match run_atomic_unit(self.sql().as_ref(), plans).await {
            Ok(AtomicRunOutcome::Committed { .. }) => Ok(entities),
            Ok(AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            }) => Err(RuntimeError::Internal(format!(
                "create_many: atomic batch rolled back at entity index {failed_op_index}: \
                 {failure:?}"
            ))),
            Err(e) => Err(RuntimeError::Internal(format!(
                "create_many: atomic batch failed: {}",
                e.0
            ))),
        }
    }

    /// One bulk entity spec's pre-write checks and its row, shared by
    /// `create_many` and [`Self::prepare_bulk_entity_plan`] so the two bulk
    /// entity paths cannot drift apart: kind, entity_type, a nonempty name,
    /// the reserved secret-gate property and the secret gate itself.
    fn validate_bulk_entity(
        &self,
        ns: &str,
        spec: &EntityCreateSpec,
        record: &str,
    ) -> RuntimeResult<Entity> {
        self.validate_entity_kind(&spec.kind)?;
        // Validate entity_type at the runtime layer via pack-installed callback.
        // When no validator is installed (bare runtime, unit tests without packs),
        // the type passes through unchanged, the same skip-when-absent pattern as
        // validate_entity_kind. The handler layer remains the primary enforcement point.
        let validated_type =
            self.validate_entity_type_for_kind(&spec.kind, spec.entity_type.as_deref())?;
        if spec.name.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("name must not be empty".into()));
        }
        crate::secret_gate::reject_reserved_secret_gate_property(spec.properties.as_ref())?;
        crate::secret_gate::check_at(&spec.name, record, "name")?;
        if let Some(d) = &spec.description {
            crate::secret_gate::check_at(d, record, "description")?;
        }
        if let Some(ref p) = spec.properties {
            crate::secret_gate::check_json_at(p, record, "properties")?;
        }
        crate::secret_gate::check_tags_at(&spec.tags, record, "tags")?;

        let mut entity =
            Entity::new(ns, &spec.kind, &spec.name).with_entity_type(validated_type.as_deref());
        if let Some(d) = &spec.description {
            entity = entity.with_description(d);
        }
        if let Some(p) = spec.properties.clone() {
            entity = entity.with_properties(p);
        }
        if !spec.tags.is_empty() {
            entity = entity.with_tags(spec.tags.clone());
        }
        Ok(entity)
    }

    /// Validate and prepare one entity item for a bulk `create(items=[...])`
    /// write: the same admission and row/FTS plan as [`Self::create_many`],
    /// with no scheduled reindex, so the vector is deferred to a later
    /// `reindex` exactly as for `create_many`. The bulk create handler uses
    /// this for every entity item so entity and note plans can join one
    /// `run_atomic_unit` call.
    pub async fn prepare_bulk_entity_plan(
        &self,
        token: &NamespaceToken,
        spec: EntityCreateSpec,
    ) -> RuntimeResult<(Entity, AtomicOpPlan)> {
        let entity = self.validate_bulk_entity(token.namespace().as_str(), &spec, "entity")?;
        let _ = self.entities(token)?;
        let _ = self.text(token)?;

        let plan = AtomicOpPlan::AddEntity(bulk_entity_plan(&entity)?);
        Ok((entity, plan))
    }

    /// Validate and prepare one note item for a bulk `create(items=[...])`
    /// write. The note goes through the preparation a singleton note create
    /// uses (`validate_head`, then `prepare_atomic_notes`: kind validation,
    /// owned-identity derivation, secret gate, salience range, row and FTS
    /// statements) with embedding switched off, so the plan writes the row
    /// and its FTS document and no vector. A later `reindex` backfills the
    /// vector, as it does for bulk entities. The caller commits the plan,
    /// alone or joined with its siblings in one `run_atomic_unit` call.
    pub async fn prepare_bulk_note_plan(
        &self,
        token: &NamespaceToken,
        spec: NoteCreateSpec,
    ) -> RuntimeResult<(Note, AtomicOpPlan)> {
        let mut candidate = Note::new(token.namespace().as_str(), &spec.kind, &spec.content);
        candidate.name = spec.name.clone();
        candidate.properties = spec.properties.clone();
        crate::note_write::validate_head(&candidate)?;
        let mut prepared = crate::atomic_message::prepare_atomic_notes(
            self,
            vec![crate::atomic_message::AtomicNoteSpec {
                token,
                id: None,
                kind: &spec.kind,
                name: spec.name.as_deref(),
                content: &spec.content,
                properties: spec.properties,
            }],
            crate::atomic_message::AtomicNoteOptions {
                salience: spec.salience,
                embed: Some(false),
                ..Default::default()
            },
        )
        .await?;
        match (prepared.notes.pop(), prepared.plans.pop()) {
            (Some(note), Some(plan)) if prepared.notes.is_empty() && prepared.plans.is_empty() => {
                Ok((note, plan))
            }
            _ => Err(RuntimeError::Internal(
                "bulk note preparation must yield exactly one note and one plan".into(),
            )),
        }
    }
}

/// One note item for [`KhiveRuntime::prepare_bulk_note_plan`], the note
/// analogue of [`EntityCreateSpec`]. The caller has already run the kind's
/// own preparation hook, so owner-kind policy such as the memory pack's
/// creation refusal happens before this point.
#[derive(Clone, Debug)]
pub struct NoteCreateSpec {
    pub kind: String,
    pub name: Option<String>,
    pub content: String,
    pub salience: Option<f64>,
    pub properties: Option<serde_json::Value>,
}

fn bulk_entity_plan(entity: &Entity) -> RuntimeResult<AddEntityPlan> {
    crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
    let mut statements = vec![PlanStatement {
        statement: entity_upsert_statement(entity),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    // The FTS insert and rowid-map insert must remain adjacent on one connection.
    statements.extend(
        insert_document_statements("fts_entities", &entity_fts_document(entity))
            .into_iter()
            .map(|statement| PlanStatement {
                statement,
                guard: None,
            }),
    );
    Ok(AddEntityPlan {
        entity_id: entity.id,
        statements,
        post_commit: PostCommitEffect::None,
    })
}

fn guarded_link_batch_failure(
    spec: &LinkSpec,
    entry_index: usize,
    missing: khive_storage::MissingEndpoints,
) -> GuardedWriteFailure {
    // Storage flags describe the canonical edge built from this spec, not the
    // caller's potentially reversed spelling of a symmetric relation.
    let (source_id, target_id) =
        canonical_edge_endpoints(spec.relation, spec.source_id, spec.target_id);
    GuardedWriteFailure {
        entry_index: Some(entry_index),
        missing_source: missing.source.then_some(source_id),
        missing_target: missing.target.then_some(target_id),
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
