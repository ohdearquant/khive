// Licensed under the Apache License, Version 2.0.

// FILE SIZE JUSTIFICATION: curation.rs holds entity/note/edge patch types alongside
// their update and merge implementations. The implementations share private helpers
// (merge_properties, namespace checks, dedup policy) that need pub(crate) access to
// runtime internals. Inline tests cover merge semantics that require direct access to
// those helpers. Split plan: extract patch types into `curation/patch.rs` and merge
// logic into `curation/merge.rs` once the dedup policy API stabilises.
//! Curation operations: entity update/merge and edge-list filter type.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_db::SqliteError;
use khive_storage::note::Note;
use khive_storage::types::{EdgeFilter, TextDocument};
use khive_storage::{EdgeRelation, Entity, SubstrateKind};
use khive_types::{Details, EdgeEndpointRule, EventKind, KhiveError};
use rusqlite::OptionalExtension;

use crate::error::{RuntimeError, RuntimeResult};
use crate::operations::{base_entity_rule_allows, canonical_edge_endpoints, endpoint_matches};
use crate::runtime::{KhiveRuntime, NamespaceToken};

/// Test-only pause point at the read/write boundary of a guarded
/// read-modify-write, so a race between two concurrent callers of the same
/// PRODUCTION entry point (not the underlying store primitive) can be
/// reproduced deterministically instead of relying on scheduler luck or
/// sleeps. A no-op unless the calling task runs inside
/// `AFTER_READ_BARRIER.scope(...)`; production code never establishes that
/// scope, so `pause_after_read` costs nothing outside these regression
/// tests, and it does not exist at all in non-test builds.
#[cfg(test)]
pub(crate) mod race_seam {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    tokio::task_local! {
        pub(crate) static AFTER_READ_BARRIER: Arc<Barrier>;
    }

    pub(crate) async fn pause_after_read() {
        if let Ok(barrier) = AFTER_READ_BARRIER.try_with(Arc::clone) {
            barrier.wait().await;
        }
    }
}

pub(crate) fn stale_note_snapshot_error(id: Uuid) -> RuntimeError {
    RuntimeError::Khive(KhiveError::conflict(format!(
        "note {id} changed concurrently after it was read; retry with fresh state"
    )))
}

pub(crate) fn stale_entity_snapshot_error(id: Uuid) -> RuntimeError {
    RuntimeError::Khive(KhiveError::conflict(format!(
        "entity {id} changed concurrently after it was read; retry with fresh state"
    )))
}

pub(crate) fn stale_edge_snapshot_error(id: Uuid) -> RuntimeError {
    RuntimeError::Khive(KhiveError::conflict(format!(
        "edge {id} changed concurrently after it was read; retry with fresh state"
    )))
}

/// Immutable embedding-registry view for one logical write.
///
/// Document byte budgets are derived from the model name at the embedding seam,
/// so retaining the exact name set keeps merge cleanup, table preparation, and
/// survivor reindexing on one plan during concurrent registration.
#[derive(Clone, Debug, Default)]
struct EmbeddingModelPlan {
    model_names: Vec<String>,
}

impl EmbeddingModelPlan {
    fn capture(runtime: &KhiveRuntime) -> Self {
        Self {
            model_names: runtime.registered_embedding_model_names(),
        }
    }

    fn is_empty(&self) -> bool {
        self.model_names.is_empty()
    }

    fn model_names(&self) -> &[String] {
        &self.model_names
    }

    fn vector_tables(&self) -> Vec<String> {
        self.model_names
            .iter()
            .map(|name| format!("vec_{}", crate::config::sanitize_key(name)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Patch for `update_entity`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `description`:
/// - `None` (outer) — leave the current description as-is
/// - `Some(None)` — clear the description (set to NULL)
/// - `Some(Some(s))` — set the description to `s`
///
/// For `properties` (deep-merge semantics):
/// - `None` — leave properties as-is
/// - `Some(value)` — deep-merge `value` into existing properties. Keys present in
///   the patch overwrite existing keys; keys absent from the patch are preserved.
///   Removing a key requires explicit replacement of the parent object (or a future
///   `unset`/`null-marker` extension).
///
/// For `tags` — replace semantics: `Some(vec)` sets tags to exactly `vec`. To add
/// a tag without losing existing tags, read the entity first, push the new tag,
/// and pass the full list back.
///
/// For `entity_type` — ADR-014 tri-state: `None` leaves the current type
/// unchanged, `Some(None)` explicitly clears it, and `Some(Some(value))`
/// validates and normalizes `value` through the installed entity-type
/// registry.
#[derive(Clone, Debug, Default)]
pub struct EntityPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub properties: Option<Value>,
    pub tags: Option<Vec<String>>,
    pub entity_type: Option<Option<String>>,
}

/// Policy used when deduplicating two entities.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityDedupMergePolicy {
    /// `into` values win on conflict. Tags are unioned. Properties from `from` fill in
    /// keys that `into` doesn't have. This is the default.
    #[default]
    PreferInto,
    /// `from` values win on conflict.
    PreferFrom,
    /// Deep-merge: object properties merge recursively. Scalar conflicts go to `into`.
    Union,
}

/// Safety-floor guard that refused an explicit entity merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityMergeGuard {
    EntityKind,
    NameSimilarity,
    ProjectCompatibility,
}

impl EntityMergeGuard {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EntityKind => "entity_kind",
            Self::NameSimilarity => "name_similarity",
            Self::ProjectCompatibility => "project_compatibility",
        }
    }
}

/// Validate the non-forced entity-merge safety floor.
pub fn validate_entity_merge_floor(into: &Entity, from: &Entity) -> Result<(), EntityMergeGuard> {
    if into.kind != from.kind {
        return Err(EntityMergeGuard::EntityKind);
    }
    if !names_are_similar(&into.name, &from.name) {
        return Err(EntityMergeGuard::NameSimilarity);
    }
    if projects_are_disjoint(into, from) {
        return Err(EntityMergeGuard::ProjectCompatibility);
    }
    Ok(())
}

/// Convert a safety-floor refusal into the merge verb's structured conflict contract.
pub fn entity_merge_guard_error(guard: EntityMergeGuard) -> RuntimeError {
    RuntimeError::Khive(
        KhiveError::conflict(format!(
            "entity merge refused by {} guard; use force=true only when the caller accepts responsibility",
            guard.as_str()
        ))
        .with_details(Details::new([
            ("guard", guard.as_str()),
            ("override", "force=true"),
        ])),
    )
}

fn names_are_similar(left: &str, right: &str) -> bool {
    let left = normalize_name(left);
    let right = normalize_name(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }

    let shorter_len = left.chars().count().min(right.chars().count());
    if shorter_len >= 3 && (left.starts_with(&right) || right.starts_with(&left)) {
        return true;
    }

    let left_trigrams = trigrams(&left);
    let right_trigrams = trigrams(&right);
    if left_trigrams.is_empty() || right_trigrams.is_empty() {
        return false;
    }
    let overlap = left_trigrams.intersection(&right_trigrams).count();
    overlap.saturating_mul(4) >= left_trigrams.len().saturating_add(right_trigrams.len())
}

fn normalize_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_whitespace() {
            pending_space = !normalized.is_empty();
        } else {
            if pending_space {
                normalized.push(' ');
                pending_space = false;
            }
            normalized.push(ch);
        }
    }
    normalized
}

fn trigrams(value: &str) -> HashSet<[char; 3]> {
    let chars: Vec<char> = value.chars().collect();
    chars
        .windows(3)
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

fn projects_are_disjoint(into: &Entity, from: &Entity) -> bool {
    let Some(into_projects) = into
        .properties
        .as_ref()
        .and_then(|properties| properties.get("projects"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(from_projects) = from
        .properties
        .as_ref()
        .and_then(|properties| properties.get("projects"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    if into_projects.is_empty() || from_projects.is_empty() {
        return false;
    }

    let (indexed, candidates) = if into_projects.len() <= from_projects.len() {
        (into_projects, from_projects)
    } else {
        (from_projects, into_projects)
    };
    let mut indexed_strings = HashSet::new();
    let mut indexed_values = HashSet::new();
    for value in indexed {
        if let Some(value) = value.as_str() {
            indexed_strings.insert(normalize_project_string(value));
        } else {
            indexed_values.insert(value.clone());
        }
    }
    !candidates.iter().any(|candidate| {
        if let Some(candidate) = candidate.as_str() {
            indexed_strings.contains(&normalize_project_string(candidate))
        } else {
            indexed_values.contains(candidate)
        }
    })
}

fn normalize_project_string(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// Strategy for merging note content when two notes are combined.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentMergeStrategy {
    #[default]
    Append,
    PreferInto,
    PreferFrom,
}

/// Result returned by `merge_entity` / `merge_note`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeSummary {
    pub kept_id: Uuid,
    pub removed_id: Uuid,
    pub edges_rewired: usize,
    /// Incident edges dropped instead of rewired because the rewired
    /// `(source, relation, target)` triple would violate the pack endpoint
    /// contract `link` enforces (khive#1216) — consistent with the existing
    /// dangling-endpoint skip behavior, never silently rewired into a
    /// contract-violating edge.
    #[serde(default)]
    pub edges_contract_skipped: usize,
    /// Full preimages for natural-key edge conflicts resolved by this merge.
    /// Each entry names the surviving row, the dropped duplicate, and every
    /// incident edge cascaded with it so the destructive step is reversible.
    #[serde(default)]
    pub edge_conflict_preimages: Vec<MergeEdgeConflictPreimage>,
    pub properties_merged: usize,
    pub tags_unioned: usize,
    pub content_appended: bool,
    pub dry_run: bool,
    /// Rows and bytes this merge materialized against the per-transaction
    /// budget, alongside the limits it was admitted under. Enforcement already
    /// happened inside the transaction; this is the observed usage.
    #[serde(default)]
    pub tx_budget: MergeTxBudgetReport,
    /// Actual embedding-input truncation observed while reindexing the survivor.
    #[serde(skip)]
    pub embedding_truncation: crate::retrieval::EmbeddingTruncationReport,
}

/// Complete stored state of an edge removed while resolving a merge conflict.
///
/// Timestamps use the storage layer's microsecond representation. `relation`
/// remains a string so a legacy row predating the closed relation enum can
/// still be captured without making an otherwise valid merge fail.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeEdgePreimage {
    pub id: Uuid,
    pub namespace: String,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: String,
    pub weight: f64,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    pub target_backend: Option<String>,
    pub metadata: Option<Value>,
}

/// One natural-key collision resolved by a direct entity or note merge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeEdgeConflictPreimage {
    pub surviving_edge_id: Uuid,
    pub dropped_edge: MergeEdgePreimage,
    /// Edges removed by the hard-delete cascade because they referenced the
    /// dropped edge as a node. Under the accepted endpoint contract these are
    /// `annotates` edges, including already-soft-deleted rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incident_edge_preimages: Vec<MergeEdgePreimage>,
}

/// Default per-transaction row cap for a direct entity/note merge. Every row
/// materialized into Rust inside the merge transaction counts: the two merge
/// records, incident edges, endpoint-contract resolutions, and conflict
/// cascade rows. Far above any legitimate single-record merge, while bounding
/// the writer hold and heap of a hub-node merge (`traverse` bounds its shared
/// read expansion at 100k rows; a merge holds the writer, so it is tighter).
const MERGE_TX_MAX_ROWS: usize = 50_000;

/// Default per-transaction aggregate byte cap across the same materialized
/// state (variable-length payloads: descriptions/content, properties, tags,
/// edge metadata, fanout table names).
const MERGE_TX_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Hard materialization limits for one merge transaction.
///
/// Enforced on the writer connection inside the merge's own `BEGIN IMMEDIATE`
/// transaction, so the counted rows are exactly the rows the merge operates
/// on — a pre-flight count on another connection could be outgrown between
/// the count and the merge. Exceeding either limit rejects the merge with the
/// observed counts before further state is materialized, and the transaction
/// rolls back. Dry runs are budgeted identically: the preview performs the
/// same reads and carries the same materialization hazard.
#[derive(Clone, Copy, Debug)]
pub struct MergeTxLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl Default for MergeTxLimits {
    fn default() -> Self {
        Self {
            max_rows: MERGE_TX_MAX_ROWS,
            max_bytes: MERGE_TX_MAX_BYTES,
        }
    }
}

/// Observed budget usage for one merge transaction (see [`MergeTxLimits`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeTxBudgetReport {
    pub rows_charged: usize,
    pub bytes_charged: usize,
    pub max_rows: usize,
    pub max_bytes: usize,
}

/// Running row/byte account for one merge transaction.
struct MergeTxBudget {
    limits: MergeTxLimits,
    rows: usize,
    bytes: usize,
}

impl MergeTxBudget {
    fn new(limits: MergeTxLimits) -> Self {
        Self {
            limits,
            rows: 0,
            bytes: 0,
        }
    }

    /// Add `rows`/`bytes` to the account; reject once either limit is passed.
    /// Callers charge each unit of state *before* retaining it, so a rejected
    /// merge never materializes more than one row past the cap.
    fn charge(&mut self, rows: usize, bytes: usize, context: &str) -> Result<(), SqliteError> {
        self.rows = self.rows.saturating_add(rows);
        self.bytes = self.bytes.saturating_add(bytes);
        if self.rows > self.limits.max_rows || self.bytes > self.limits.max_bytes {
            return Err(SqliteError::InvalidData(format!(
                "merge transaction budget exceeded while {context}: {} rows / {} bytes \
                 materialized (limits {} rows / {} bytes); the merge was rejected before \
                 materializing further state — curate the incident edges down or merge in \
                 smaller steps",
                self.rows, self.bytes, self.limits.max_rows, self.limits.max_bytes
            )));
        }
        Ok(())
    }

    fn report(&self) -> MergeTxBudgetReport {
        MergeTxBudgetReport {
            rows_charged: self.rows,
            bytes_charged: self.bytes,
            max_rows: self.limits.max_rows,
            max_bytes: self.limits.max_bytes,
        }
    }
}

/// Fixed overhead approximates the id/timestamp/weight columns; variable
/// payloads are counted at their stored length.
fn edge_row_budget_bytes(edge: &EdgeRow) -> usize {
    96 + edge.namespace.len()
        + edge.relation.len()
        + edge.target_backend.as_deref().map_or(0, str::len)
        + edge.metadata.as_deref().map_or(0, str::len)
}

/// Patch for `update_edge`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `properties` — replacement semantics (not deep merge): `Some(value)` replaces
/// the entire metadata object. `None` leaves metadata unchanged.
#[derive(Clone, Debug, Default)]
pub struct EdgePatch {
    pub relation: Option<EdgeRelation>,
    pub weight: Option<f64>,
    pub properties: Option<Value>,
}

/// Patch for `update_note`. Only `Some(_)` fields are applied; `None` means "leave unchanged".
///
/// For `salience`/`decay_factor`:
/// - `None` (outer) — leave unchanged
/// - `Some(None)` — clear the value
/// - `Some(Some(v))` — set to v
#[derive(Clone, Debug, Default)]
pub struct NotePatch {
    pub name: Option<Option<String>>,
    pub content: Option<String>,
    pub salience: Option<Option<f64>>,
    pub decay_factor: Option<Option<f64>>,
    pub properties: Option<Value>,
    pub(crate) kind_status: Option<String>,
}

impl NotePatch {
    /// Construct a `NotePatch` from the public fields only.
    /// Use this from external crates; `kind_status` is set to `None`.
    pub fn new(
        name: Option<Option<String>>,
        content: Option<String>,
        salience: Option<Option<f64>>,
        decay_factor: Option<Option<f64>>,
        properties: Option<Value>,
    ) -> Self {
        Self {
            name,
            content,
            salience,
            decay_factor,
            properties,
            kind_status: None,
        }
    }
}

/// Filter for `list_edges` / `count_edges`.
#[derive(Clone, Debug, Default)]
pub struct EdgeListFilter {
    pub source_id: Option<Uuid>,
    pub target_id: Option<Uuid>,
    /// Empty = any relation.
    pub relations: Vec<EdgeRelation>,
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
}

impl From<EdgeListFilter> for EdgeFilter {
    fn from(f: EdgeListFilter) -> Self {
        EdgeFilter {
            source_ids: f.source_id.into_iter().collect(),
            target_ids: f.target_id.into_iter().collect(),
            relations: f.relations,
            min_weight: f.min_weight,
            max_weight: f.max_weight,
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Private types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntityMergeValidation {
    LegacyKind,
    SafetyFloor,
    Forced,
}

#[derive(Debug)]
enum EntityMergeRefusal {
    LegacyKind {
        into_id: Uuid,
        into_kind: String,
        from_id: Uuid,
        from_kind: String,
    },
    SafetyFloor(EntityMergeGuard),
}

impl EntityMergeRefusal {
    fn into_runtime_error(self) -> RuntimeError {
        match self {
            Self::LegacyKind {
                into_id,
                into_kind,
                from_id,
                from_kind,
            } => RuntimeError::InvalidInput(format!(
                "cannot merge entities of different kinds: into={into_id} ({into_kind}), \
                 from={from_id} ({from_kind}); merge requires both entities to share the same kind"
            )),
            Self::SafetyFloor(guard) => entity_merge_guard_error(guard),
        }
    }
}

#[derive(Debug)]
enum MergeEntitySqlError {
    Sqlite(SqliteError),
    Refusal(EntityMergeRefusal),
}

impl std::fmt::Display for MergeEntitySqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => std::fmt::Display::fmt(error, f),
            Self::Refusal(_) => f.write_str("entity merge refused by transactional policy"),
        }
    }
}

impl std::error::Error for MergeEntitySqlError {}

impl From<SqliteError> for MergeEntitySqlError {
    fn from(error: SqliteError) -> Self {
        Self::Sqlite(error)
    }
}

impl From<rusqlite::Error> for MergeEntitySqlError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(SqliteError::Rusqlite(error))
    }
}

fn map_merge_entity_storage_error(error: khive_storage::StorageError) -> RuntimeError {
    match error {
        khive_storage::StorageError::Driver {
            capability,
            operation,
            source,
        } => match source.downcast::<MergeEntitySqlError>() {
            Ok(error) => match *error {
                MergeEntitySqlError::Sqlite(error) => RuntimeError::Sqlite(error),
                MergeEntitySqlError::Refusal(error) => error.into_runtime_error(),
            },
            Err(source) => RuntimeError::Storage(khive_storage::StorageError::Driver {
                capability,
                operation,
                source,
            }),
        },
        error => RuntimeError::Storage(error),
    }
}

// REASON: EdgeRow fields are populated via rusqlite row mapping. The struct is fully
// constructed even when not all fields are read back after construction. The complete
// field mapping guards against column-order bugs when the schema changes.
#[derive(Clone)]
struct EdgeRow {
    id: Uuid,
    /// The edge's own attribution namespace (khive#1236) — may differ from the
    /// merge's target namespace, since by-ID edge endpoints are namespace-agnostic
    /// (ADR-007 Rev 6) and an edge is stamped with its *creator's* namespace, not
    /// either endpoint's. All row-scoped SQL against this edge (conflict probe,
    /// update, delete) must key off this field, never the merge's `namespace` arg.
    namespace: String,
    source_id: Uuid,
    target_id: Uuid,
    relation: String,
    weight: f64,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
    target_backend: Option<String>,
    metadata: Option<String>,
}

fn edge_row_preimage(edge: &EdgeRow) -> Result<MergeEdgePreimage, SqliteError> {
    let metadata = edge
        .metadata
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
    Ok(MergeEdgePreimage {
        id: edge.id,
        namespace: edge.namespace.clone(),
        source_id: edge.source_id,
        target_id: edge.target_id,
        relation: edge.relation.clone(),
        weight: edge.weight,
        created_at: edge.created_at,
        updated_at: edge.updated_at,
        deleted_at: edge.deleted_at,
        target_backend: edge.target_backend.clone(),
        metadata,
    })
}

/// Capture every row that the accepted hard-edge-delete cascade would remove
/// when `root_edge_id` is purged. The traversal is recursive because an
/// `annotates` edge may itself be an annotation target. Rows that also touch a
/// merge participant use their transaction-start snapshot from `original_edges`
/// so the preimage never reflects an earlier rewire in the same merge.
fn collect_conflict_incident_edge_preimages(
    conn: &rusqlite::Connection,
    root_edge_id: Uuid,
    original_edges: &HashMap<Uuid, EdgeRow>,
    budget: &mut MergeTxBudget,
) -> Result<Vec<MergeEdgePreimage>, SqliteError> {
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));
    let mut queue = VecDeque::from([root_edge_id]);
    let mut seen = HashSet::from([root_edge_id]);
    let mut preimages = Vec::new();

    while let Some(target_edge_id) = queue.pop_front() {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1 OR target_id = ?1 ORDER BY id",
        )?;
        let mut rows = stmt.query(rusqlite::params![target_edge_id.to_string()])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(
                1,
                edge_row_budget_bytes(&edge),
                "collecting conflict cascade rows",
            )?;
            if !seen.insert(edge.id) {
                continue;
            }
            let preimage = match original_edges.get(&edge.id) {
                Some(original) => edge_row_preimage(original)?,
                None => edge_row_preimage(&edge)?,
            };
            queue.push_back(edge.id);
            preimages.push(preimage);
        }
    }

    Ok(preimages)
}

fn delete_conflict_incident_edges(
    conn: &rusqlite::Connection,
    preimages: &[MergeEdgePreimage],
) -> Result<(), SqliteError> {
    for edge in preimages.iter().rev() {
        conn.execute(
            khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
            rusqlite::params![&edge.namespace, edge.id.to_string()],
        )?;
    }
    Ok(())
}

/// Resolves the substrate (`"entity"` or `"note"`), kind, and entity_type (entities
/// only) of an edge endpoint by id, namespace-agnostically — by-ID resolution is
/// namespace-agnostic by design (ADR-007 Rev 6), and an edge's non-merging endpoint
/// may live in any namespace. Returns `None` if `id` resolves to neither table
/// (e.g. a hard-deleted or otherwise absent record); callers must treat that as
/// "the endpoint contract cannot be evaluated" and drop the edge rather than
/// silently allow it through.
/// `(substrate, kind, entity_type)` for a resolved merge-edge endpoint.
type MergeEdgeEndpointInfo = (&'static str, String, Option<String>);

fn resolve_merge_edge_endpoint(
    conn: &rusqlite::Connection,
    id: Uuid,
) -> Result<Option<MergeEdgeEndpointInfo>, SqliteError> {
    let id_str = id.to_string();
    if let Some((kind, entity_type)) = conn
        .query_row(
            "SELECT kind, entity_type FROM entities WHERE id = ?1",
            rusqlite::params![&id_str],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?
    {
        return Ok(Some(("entity", kind, entity_type)));
    }
    if let Some(kind) = conn
        .query_row(
            "SELECT kind FROM notes WHERE id = ?1",
            rusqlite::params![&id_str],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?
    {
        return Ok(Some(("note", kind, None)));
    }
    Ok(None)
}

/// [`resolve_merge_edge_endpoint`] with the resolved row charged against the
/// merge transaction budget — endpoint resolution reads one row per non-merging
/// endpoint, so a hub merge's contract checks are part of its materialization.
fn resolve_merge_edge_endpoint_budgeted(
    conn: &rusqlite::Connection,
    id: Uuid,
    budget: &mut MergeTxBudget,
) -> Result<Option<MergeEdgeEndpointInfo>, SqliteError> {
    let info = resolve_merge_edge_endpoint(conn, id)?;
    if let Some((_, kind, entity_type)) = &info {
        budget.charge(
            1,
            kind.len() + entity_type.as_deref().map_or(0, str::len),
            "resolving rewire endpoint contracts",
        )?;
    }
    Ok(info)
}

/// `true` if `(src_sub, src_kind, src_type) -[relation]-> (tgt_sub, tgt_kind, tgt_type)`
/// is permitted under the base ADR-002 entity allowlist or a pack-declared
/// `EdgeEndpointRule` — the exact same `endpoint_matches` semantics `link`'s
/// `validate_edge_relation_endpoints` applies (khive-runtime/src/operations.rs),
/// reused here rather than re-derived, per the #543/#621 lesson that a parallel
/// matcher drifts out of sync with the validator.
///
/// `annotates` is exempt: its source-must-be-a-note constraint is enforced at
/// edge creation and unchanged by rewiring (an entity merge only ever rewires
/// its unfiltered target; a note merge rewiring the source substitutes another
/// note), and its target may be any substrate. Callers short-circuit `annotates`
/// before endpoint resolution — an annotates target may be an event or an edge,
/// which `resolve_merge_edge_endpoint` cannot resolve; the exemption here is
/// kept as defense in depth.
// REASON: the two endpoints each need substrate/kind/entity_type independently —
// collapsing them into a tuple/struct would obscure which side is which at call
// sites that already pass them as separate locals.
#[allow(clippy::too_many_arguments)]
fn merge_rewire_endpoint_contract_allows(
    pack_rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
    src_sub: &str,
    src_kind: &str,
    src_type: Option<&str>,
    tgt_sub: &str,
    tgt_kind: &str,
    tgt_type: Option<&str>,
) -> bool {
    if relation == EdgeRelation::Annotates {
        return true;
    }
    // Same-substrate relations permit any note→note pair unconditionally,
    // matching `validate_edge_relation_endpoints`'s `(Note, Note) => {}` arm.
    if src_sub == "note" && tgt_sub == "note" && crate::pack::is_special_relation(relation) {
        return true;
    }
    if src_sub == "entity"
        && tgt_sub == "entity"
        && base_entity_rule_allows(src_kind, relation, tgt_kind)
    {
        return true;
    }
    pack_rules.iter().any(|r| {
        r.relation == relation
            && endpoint_matches(&r.source, src_sub, src_kind, src_type)
            && endpoint_matches(&r.target, tgt_sub, tgt_kind, tgt_type)
    })
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl KhiveRuntime {
    /// Patch-style entity update.
    ///
    /// Only fields set to `Some(_)` are changed. Re-indexes FTS5 (and vectors if configured)
    /// when `name`, `description`, or `entity_type` changes; skips re-indexing for
    /// property/tag-only patches.
    ///
    /// Returns `RuntimeError::NotFound` if the entity does not exist or belongs to a different
    /// namespace. Namespace isolation is enforced at the runtime layer.
    /// Computes the patched `Entity`, `reindex_required`, and `changed_fields` without
    /// writing anything, so both the normal write path and the atomic-prepare path
    /// share one source of truth for what a patched entity looks like.
    pub(crate) async fn prepare_update_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<(Entity, bool, Vec<&'static str>, i64, Option<i64>)> {
        crate::secret_gate::reject_reserved_secret_gate_property(patch.properties.as_ref())?;
        if let Some(ref name) = patch.name {
            crate::secret_gate::check(name)?;
        }
        if let Some(Some(ref desc)) = patch.description {
            crate::secret_gate::check(desc)?;
        }
        if let Some(ref props) = patch.properties {
            crate::secret_gate::check_json(props)?;
        }
        if let Some(ref tags) = patch.tags {
            crate::secret_gate::check_tags(tags)?;
        }
        let store = self.entities(token)?;
        let mut entity = store
            .get_entity(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("entity {id}")))?;
        let expected_updated_at = entity.updated_at;
        let expected_deleted_at = entity.deleted_at;
        #[cfg(test)]
        race_seam::pause_after_read().await;

        // ADR-014 tri-state: outer `None` = unchanged; `Some(None)` = explicit
        // clear (no vocabulary validation — there is no value to validate);
        // `Some(Some(raw))` = set, validated and normalized.
        let validated_entity_type = match &patch.entity_type {
            Some(None) => Some(None),
            Some(Some(raw)) => Some(Some(
                self.validate_entity_type_for_kind(&entity.kind, Some(raw))?
                    .expect("set branch always yields a normalized value"),
            )),
            None => None,
        };

        let mut reindex_required = false;
        let mut changed_fields: Vec<&'static str> = Vec::new();

        if let Some(name) = patch.name {
            reindex_required |= entity.name != name;
            entity.name = name;
            changed_fields.push("name");
        }
        if let Some(desc_patch) = patch.description {
            reindex_required |= entity.description != desc_patch;
            entity.description = desc_patch;
            changed_fields.push("description");
        }
        if let Some(props) = patch.properties {
            let (merged, _) = merge_properties(
                &entity.properties,
                &Some(props),
                EntityDedupMergePolicy::PreferFrom,
            );
            entity.properties = merged;
            changed_fields.push("properties");
        }
        if let Some(tags) = patch.tags {
            entity.tags = tags;
            changed_fields.push("tags");
        }
        if let Some(entity_type) = validated_entity_type {
            reindex_required |= entity.entity_type != entity_type;
            entity.entity_type = entity_type;
            changed_fields.push("entity_type");
        }

        // `updated_at` is also the optimistic-concurrency revision for
        // full-entity replacement. Make it strictly advance even when two
        // operations land inside one clock microsecond. Saturation is not a
        // valid fallback: reusing i64::MAX would make the CAS accept a write
        // without advancing its revision.
        let minimum_updated_at = expected_updated_at.checked_add(1).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "entity {id} updated_at is already at i64::MAX and cannot advance"
            ))
        })?;
        entity.updated_at = chrono::Utc::now()
            .timestamp_micros()
            .max(minimum_updated_at);
        Ok((
            entity,
            reindex_required,
            changed_fields,
            expected_updated_at,
            expected_deleted_at,
        ))
    }

    pub async fn update_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<Entity> {
        Ok(self
            .update_entity_with_embedding_report(token, id, patch)
            .await?
            .0)
    }

    pub async fn update_entity_with_embedding_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        let (entity, reindex_required, changed_fields, expected_updated_at, expected_deleted_at) =
            self.prepare_update_entity(token, id, patch).await?;

        let store = self.entities(token)?;
        let persisted = store
            .replace_entity_if_unchanged(entity.clone(), expected_updated_at, expected_deleted_at)
            .await?;
        if !persisted {
            return Err(stale_entity_snapshot_error(id));
        }

        let embedding_report = if reindex_required {
            self.reindex_entity(token, &entity).await?
        } else {
            crate::retrieval::EmbeddingTruncationReport::default()
        };

        let event_token =
            token.with_namespace(crate::Namespace::parse(&entity.namespace).map_err(|error| {
                RuntimeError::Internal(format!("entity namespace invalid: {error}"))
            })?);
        let event_store = self.events(&event_token)?;
        let event = khive_storage::event::Event::new(
            entity.namespace.clone(),
            "update",
            EventKind::EntityUpdated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(entity.id)
        .with_payload(serde_json::json!({
            "id": entity.id,
            "namespace": entity.namespace,
            "changed_fields": changed_fields,
        }));
        event_store.append_event(event).await.map_err(|e| {
            RuntimeError::Internal(format!("update_entity: event store write failed: {e}"))
        })?;

        Ok((entity, embedding_report))
    }

    /// Merge `from_id` into `into_id`.
    ///
    /// All edges incident to `from_id` are rewired to `into_id`. Self-loops that would
    /// result from the rewire are dropped. Properties and tags are merged per `strategy`.
    /// `from_id` is tombstoned with merge provenance and removed from indexes. Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating any rows.
    ///
    /// Atomic: all SQL (entity reads/writes, edge rewires, FTS updates, vec-index delete)
    /// runs on a single pool connection inside one `BEGIN IMMEDIATE` transaction via
    /// `merge_entity_sql`. If embedding vectors are configured, the vector re-insert for
    /// `into_id` is performed after the transaction (requires async embedding computation).
    pub async fn merge_entity(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_entity_with_reason(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            None,
        )
        .await
    }

    /// Merge `from_id` into `into_id` and include an optional reason in the audit event.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_entity_with_reason(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_entity_with_validation(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            reason,
            EntityMergeValidation::LegacyKind,
        )
        .await
    }

    /// Merge two entities with an explicit override for the entity safety floor.
    ///
    /// Non-forced calls enforce entity kind, name similarity, and project compatibility
    /// against the rows reread inside the merge transaction. Legacy merge methods retain
    /// their historical same-kind-only policy.
    /// A non-dry-run override is recorded as `force: true` in the merge event.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, audit-reason, and force fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_entity_with_reason_and_force(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
        force: bool,
    ) -> RuntimeResult<MergeSummary> {
        let validation = if force {
            EntityMergeValidation::Forced
        } else {
            EntityMergeValidation::SafetyFloor
        };
        self.merge_entity_with_validation(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            reason,
            validation,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn merge_entity_with_validation(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
        validation: EntityMergeValidation,
    ) -> RuntimeResult<MergeSummary> {
        if let Some(reason) = reason.as_deref() {
            crate::secret_gate::check(reason)?;
        }
        if into_id == from_id {
            return Err(RuntimeError::InvalidInput(
                "cannot merge an entity into itself".into(),
            ));
        }
        let ns = token.namespace().as_str().to_owned();
        let fts_table = "fts_entities".to_string();
        // One immutable registry view governs transactional deletion, table
        // preparation, and survivor reindex. A late model belongs to a later
        // write/backfill rather than only one leg of this merge.
        let embedding_plan = EmbeddingModelPlan::capture(self);
        let vec_tables = embedding_plan.vector_tables();
        // Loaded once here (sync, cheap) so the rewire loop can evaluate the
        // endpoint contract without an async round-trip per edge (khive#1216).
        let pack_rules = self.pack_edge_rules();

        // Ensure all required tables exist (idempotent DDL) before the transaction.
        let _ = self.entities(token)?;
        let _ = self.graph(token)?;
        let _ = self.text(token)?;
        // vectors_for_model (not the default-model-only self.vectors()) so
        // custom-only runtimes (no default embedding_model) still get DDL primed.
        for model_name in embedding_plan.model_names() {
            let _ = self.vectors_for_model(token, model_name)?;
        }

        let pool = self.backend().pool_arc();
        // When the write queue is enabled, route this multi-statement merge through
        // the single-writer task instead of the pool's writer mutex. A lookup
        // failure degrades to the legacy mutex path rather than failing the merge.
        let writer_task = pool.writer_task_handle().ok().flatten();

        let (mut summary, updated_entity) = if let Some(writer_task) = writer_task {
            writer_task
                .send(move |conn| {
                    merge_entity_sql(
                        conn,
                        ns,
                        fts_table,
                        vec_tables,
                        into_id,
                        from_id,
                        strategy,
                        content_strategy,
                        dry_run,
                        pack_rules,
                        validation,
                        MergeTxLimits::default(),
                    )
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Entities,
                            "merge_entity",
                            e,
                        )
                    })
                })
                .await
                .map_err(map_merge_entity_storage_error)?
        } else {
            tokio::task::spawn_blocking(move || {
                let guard = pool.writer()?;
                let mut refusal = None;
                let result = guard.transaction(|conn| {
                    merge_entity_sql(
                        conn,
                        ns,
                        fts_table,
                        vec_tables,
                        into_id,
                        from_id,
                        strategy,
                        content_strategy,
                        dry_run,
                        pack_rules,
                        validation,
                        MergeTxLimits::default(),
                    )
                    .map_err(|error| match error {
                        MergeEntitySqlError::Sqlite(error) => error,
                        MergeEntitySqlError::Refusal(error) => {
                            refusal = Some(error);
                            SqliteError::InvalidData(
                                "entity merge refused by transactional policy".to_string(),
                            )
                        }
                    })
                });
                match refusal {
                    Some(error) => Err(error.into_runtime_error()),
                    None => result.map_err(RuntimeError::from),
                }
            })
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))??
        };

        // Emitted only after the transaction has committed, so the log write
        // never extends the writer hold the budget exists to bound.
        if !dry_run {
            tracing::info!(
                into_id = %summary.kept_id,
                from_id = %summary.removed_id,
                budget_rows = summary.tx_budget.rows_charged,
                budget_bytes = summary.tx_budget.bytes_charged,
                budget_max_rows = summary.tx_budget.max_rows,
                budget_max_bytes = summary.tx_budget.max_bytes,
                "merge_entity: transaction materialization budget"
            );
        }

        // FTS and vec-deletes already committed inside the transaction above;
        // only the embedding re-insert needs an async step outside it.
        if !dry_run && !embedding_plan.is_empty() {
            summary.embedding_truncation = self
                .reindex_entity_with_plan(token, &updated_entity, &embedding_plan)
                .await?;
        }

        // Dry-run is a read-only preview: it must not append a merge event.
        if !dry_run {
            let event_token =
                token.with_namespace(crate::Namespace::parse(&updated_entity.namespace).map_err(
                    |error| RuntimeError::Internal(format!("entity namespace invalid: {error}")),
                )?);
            let event_store = self.events(&event_token)?;
            // Mirror the wire-level strategy spelling from MergeParams so consumers
            // can round-trip the policy string back into a request.
            let policy_str = match strategy {
                EntityDedupMergePolicy::PreferInto => "prefer_into",
                EntityDedupMergePolicy::PreferFrom => "prefer_from",
                EntityDedupMergePolicy::Union => "union",
            };
            let mut payload = serde_json::json!({
                "into_id": summary.kept_id,
                "from_id": summary.removed_id,
                "policy": policy_str,
                "content_strategy": format!("{:?}", content_strategy),
                "edges_rewired": summary.edges_rewired,
                "edges_contract_skipped": summary.edges_contract_skipped,
                "edge_conflict_preimages": &summary.edge_conflict_preimages,
            });
            if let Some(reason) = reason {
                payload["reason"] = serde_json::Value::String(reason);
            }
            if validation == EntityMergeValidation::Forced {
                payload["force"] = serde_json::Value::Bool(true);
            }
            let event = khive_storage::event::Event::new(
                updated_entity.namespace.clone(),
                "merge",
                EventKind::EntityMerged,
                SubstrateKind::Entity,
                "",
            )
            .with_target(summary.kept_id)
            .with_payload(payload);
            event_store.append_event(event).await.map_err(|e| {
                RuntimeError::Internal(format!("merge_entity: event store write failed: {e}"))
            })?;
        }

        Ok(summary)
    }

    // ---- Internal helpers ----

    /// Re-upsert FTS5 document and vector(s) for the entity across all registered models.
    ///
    /// Uses `entity.namespace` — the authoritative namespace stored on the record — rather
    /// than the caller-supplied `namespace` parameter. This prevents a cross-namespace
    /// reindex from writing the search document into the wrong namespace's FTS index.
    ///
    /// Best-effort for vectors: if embedding or inserting for a particular model fails,
    /// logs a warning and continues to the next model. The FTS step is fail-closed
    /// (propagates error). Callers (update_entity, merge_entity) have already committed
    /// the entity row, so a partial embed miss leaves a stale vector rather than
    /// rolling back the update — acceptable because SqliteVecStore::insert is an upsert
    /// (the prior vector stays intact on failure, keeping the record searchable).
    pub(crate) async fn reindex_entity(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        let embedding_plan = EmbeddingModelPlan::capture(self);
        self.reindex_entity_with_plan(token, entity, &embedding_plan)
            .await
    }

    async fn reindex_entity_with_plan(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        embedding_plan: &EmbeddingModelPlan,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        // Use entity.namespace (authoritative) rather than token.namespace().as_str() (caller claim).
        let ns = entity.namespace.clone();
        let doc = entity_fts_document(entity);
        let embed_body = doc.body.clone();
        self.text(token)?.upsert_document(doc).await?;

        let mut report = crate::retrieval::EmbeddingTruncationReport::default();
        for model_name in embedding_plan.model_names() {
            match self
                .embed_document_with_model_outcome_for_token(token, model_name, &embed_body)
                .await
            {
                Ok(outcome) => {
                    report.observe(&outcome);
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => {
                            if let Err(e) = vs
                                .insert(
                                    entity.id,
                                    SubstrateKind::Entity,
                                    &ns,
                                    "entity.body",
                                    vec![outcome.vector],
                                )
                                .await
                            {
                                tracing::warn!(
                                    model = model_name,
                                    id = %entity.id,
                                    "reindex_entity: vector insert failed, skipping model: {e}"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                model = model_name,
                                id = %entity.id,
                                "reindex_entity: could not access vector store for model, skipping: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        model = model_name,
                        id = %entity.id,
                        "reindex_entity: embed failed for model, skipping: {e}"
                    );
                }
            }
        }

        Ok(report)
    }

    /// Remove an entity from FTS5 and vector indexes across all registered models.
    pub(crate) async fn remove_from_indexes(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<()> {
        let ns = token.namespace().as_str().to_owned();
        self.text(token)?.delete_document(&ns, id).await?;
        for model_name in self.registered_embedding_model_names() {
            self.vectors_for_model(token, &model_name)?
                .delete(id)
                .await?;
        }
        Ok(())
    }

    /// Re-upsert FTS5 document and vector(s) for the note across all registered models.
    ///
    /// Best-effort for vectors: mirrors reindex_entity's warn-and-continue policy.
    pub(crate) async fn reindex_note(
        &self,
        token: &NamespaceToken,
        note: &khive_storage::note::Note,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        let embedding_plan = EmbeddingModelPlan::capture(self);
        self.reindex_note_with_plan(token, note, &embedding_plan)
            .await
    }

    async fn reindex_note_with_plan(
        &self,
        token: &NamespaceToken,
        note: &khive_storage::note::Note,
        embedding_plan: &EmbeddingModelPlan,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        self.text_for_notes(token)?
            .upsert_document(note_fts_document(note))
            .await?;

        let ns = note.namespace.clone();
        let mut report = crate::retrieval::EmbeddingTruncationReport::default();
        for model_name in embedding_plan.model_names() {
            match self
                .embed_document_with_model_outcome_for_token(
                    token,
                    model_name,
                    note_embedding_text_ref(note),
                )
                .await
            {
                Ok(outcome) => {
                    report.observe(&outcome);
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => {
                            if let Err(e) = vs
                                .insert(
                                    note.id,
                                    SubstrateKind::Note,
                                    &ns,
                                    "note.content",
                                    vec![outcome.vector],
                                )
                                .await
                            {
                                tracing::warn!(
                                    model = model_name,
                                    id = %note.id,
                                    "reindex_note: vector insert failed, skipping model: {e}"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                model = model_name,
                                id = %note.id,
                                "reindex_note: could not access vector store for model, skipping: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        model = model_name,
                        id = %note.id,
                        "reindex_note: embed failed for model, skipping: {e}"
                    );
                }
            }
        }
        Ok(report)
    }

    /// Apply a note patch to exactly the supplied read snapshot without
    /// fetching the row again. The caller must persist it through
    /// [`Self::update_note_from_snapshot_with_embedding_report`] or a write
    /// plan guarded by the snapshot's `updated_at`/`deleted_at` values.
    pub(crate) async fn prepare_update_note_from_snapshot(
        &self,
        _token: &NamespaceToken,
        mut note: khive_storage::note::Note,
        patch: NotePatch,
    ) -> RuntimeResult<(khive_storage::note::Note, bool)> {
        crate::secret_gate::reject_reserved_secret_gate_property(patch.properties.as_ref())?;
        if let Some(ref content) = patch.content {
            crate::secret_gate::check(content)?;
        }
        if let Some(Some(ref name)) = patch.name {
            crate::secret_gate::check(name)?;
        }
        if let Some(ref props) = patch.properties {
            crate::secret_gate::check_json(props)?;
        }

        reject_pack_managed_schedule_mutation(&note, "update")?;

        let mut text_changed = false;

        if let Some(name_patch) = patch.name {
            text_changed |= note.name != name_patch;
            note.name = name_patch;
        }
        if let Some(content) = patch.content {
            text_changed |= note.content != content;
            note.content = content;
        }
        if let Some(salience_patch) = patch.salience {
            // Reject invalid salience rather than silently clamping caller input.
            if let Some(s) = salience_patch {
                if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "salience must be a finite value in [0.0, 1.0]; got {s}"
                    )));
                }
            }
            note.salience = salience_patch;
        }
        if let Some(decay_patch) = patch.decay_factor {
            // Reject invalid decay_factor rather than silently clamping caller input.
            if let Some(d) = decay_patch {
                if !d.is_finite() || d < 0.0 {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "decay_factor must be a finite value >= 0.0; got {d}"
                    )));
                }
            }
            note.decay_factor = decay_patch;
        }
        if let Some(props) = patch.properties {
            // ADR-056 makes these three properties transport evidence owned
            // exclusively by `comm.ingest`. This check lives at the runtime
            // patch seam, not only in comm's shared-CRUD hook, because direct
            // Rust callers and atomic update preparation both arrive here
            // without dispatching that hook. Scope it to `message`: the same
            // JSON names remain ordinary caller metadata on every other kind.
            if note.kind == "message" {
                if !props.is_object() {
                    return Err(RuntimeError::InvalidInput(
                        "properties on a `message` note must be patched with an object: a \
                         non-object patch would replace the transport-owned quarantine and \
                         channel provenance established by `comm.ingest`"
                            .into(),
                    ));
                }
                if let Some(named) = message_transport_owned_property_named_in(&props) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "`{named}` is transport-owned on a `message` note and cannot be patched; \
                         only `comm.ingest` may establish quarantine disposition and channel \
                         provenance"
                    )));
                }
            }
            // On a pack-owned note kind, the properties in
            // `OWNER_ESTABLISHED_PROPERTIES` are established by the owning pack
            // and read back by it to decide something structural — who wrote
            // the record and when, which author-side record it copies, which
            // conversation it belongs to. A caller cannot patch them here.
            // Only a patch that *names* one of them is refused, and naming is
            // the exact test: the merge below is `PreferFrom`, so a patch that
            // names an owned key would overwrite it while a patch that does
            // not name it leaves it intact. Every other key still merges
            // normally — arbitrary metadata on a pack-owned record (a
            // `blocked_on` note on a `task`) has no other write path and must
            // keep working.
            if self.is_pack_owned_note_kind(&note.kind) {
                // A non-object patch names nothing, so it slips past the
                // named-key check below and then takes `merge_json`'s
                // non-object `PreferFrom` arm, which replaces the whole
                // property object rather than merging into it — erasing
                // every owned key. Refused on every pack-owned kind, not only
                // rows that currently carry an owned key, so an identical
                // call cannot succeed or fail on state the caller cannot see.
                if !props.is_object() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "properties on a `{}` note must be patched with an object: a non-object \
                         patch names no key, so it would replace the whole property object rather \
                         than merging into it. Pass an object containing the keys you intend to \
                         set.",
                        note.kind
                    )));
                }
                if let Some(named) = owner_established_property_named_in(&props) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "`{named}` is not patchable on a `{}` note: the pack that owns this \
                         kind establishes it and reads it back — to decide how the record is \
                         attributed and grouped, or to reproduce it verbatim when the record \
                         is re-emitted — so it is written by the owner and immutable to a \
                         caller patch. Patch any other property key here, or omit \
                         `{named}` from this patch.",
                        note.kind
                    )));
                }
            }
            let (merged, _) = merge_properties(
                &note.properties,
                &Some(props),
                EntityDedupMergePolicy::PreferFrom,
            );
            note.properties = merged;
        }
        if let Some(status) = patch.kind_status {
            note.status = status;
        }

        // `updated_at` is also the optimistic-concurrency revision for
        // full-note replacement. Make it strictly advance even when two
        // operations land inside one clock microsecond. Saturation is not a
        // valid fallback: reusing i64::MAX would make the CAS accept a write
        // without advancing its revision.
        let minimum_updated_at = note.updated_at.checked_add(1).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "note {} updated_at is already at i64::MAX and cannot advance",
                note.id
            ))
        })?;
        note.updated_at = chrono::Utc::now()
            .timestamp_micros()
            .max(minimum_updated_at);
        Ok((note, text_changed))
    }

    /// Patch-style note update.
    pub async fn update_note(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: NotePatch,
    ) -> RuntimeResult<khive_storage::note::Note> {
        Ok(self
            .update_note_with_embedding_report(token, id, patch)
            .await?
            .0)
    }

    pub async fn update_note_with_embedding_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: NotePatch,
    ) -> RuntimeResult<(
        khive_storage::note::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let snapshot = self
            .notes(token)?
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        self.update_note_from_snapshot_with_embedding_report(token, snapshot, patch)
            .await
    }

    /// Patch and persist one note from a caller-owned read snapshot.
    ///
    /// This is the canonical seam for kind hooks that normalize coupled
    /// fields from the current note. The same snapshot feeds normalization,
    /// patch application, and the compare-and-swap write; a concurrent note
    /// change therefore refuses the write instead of persisting derivations
    /// computed from stale state.
    pub async fn update_note_from_snapshot_with_embedding_report(
        &self,
        token: &NamespaceToken,
        snapshot: khive_storage::note::Note,
        patch: NotePatch,
    ) -> RuntimeResult<(
        khive_storage::note::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let expected_updated_at = snapshot.updated_at;
        let expected_deleted_at = snapshot.deleted_at;
        let id = snapshot.id;
        let store = self.notes(token)?;
        let current = store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        if current != snapshot {
            return Err(stale_note_snapshot_error(id));
        }
        let (note, text_changed) = self
            .prepare_update_note_from_snapshot(token, snapshot, patch)
            .await?;

        let persisted = store
            .replace_note_if_unchanged(note.clone(), expected_updated_at, expected_deleted_at)
            .await?;
        if !persisted {
            return Err(stale_note_snapshot_error(id));
        }

        let embedding_report = if text_changed {
            let report = self.reindex_note(token, &note).await?;
            // Notify any pack-owned vector cache (e.g. a warm ANN index) that this
            // note's embedding changed, via a generic hook so khive-runtime/pack-kg
            // never take a dependency on the consuming pack. No-op if unregistered.
            self.fire_note_mutation_hook(&note.kind, note.id).await;
            report
        } else {
            crate::retrieval::EmbeddingTruncationReport::default()
        };

        Ok((note, embedding_report))
    }

    /// Claim `external_id` on an outbound `message` note through the
    /// ADR-124-sanctioned store-level one-key atomic path, bypassing the
    /// caller-facing owner-established-property refusal in
    /// [`Self::update_note`] (and its crate-internal prepare path). This is deliberately
    /// NOT exposed through any registered verb (ADR-124's stated bound): it is
    /// reachable only from pack/runtime code that owns outbox bookkeeping for
    /// the `message` note kind.
    ///
    /// Refuses (returns `Err`, never writes) unless the live row is a
    /// `message` note, `properties.direction == "outbound"`, and
    /// `properties.external_id` is currently absent or empty.
    pub async fn claim_outbound_message_external_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        external_id: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let store = self.notes(token)?;
        let note = store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        if note.kind != "message" {
            return Err(RuntimeError::InvalidInput(format!(
                "external_id can only be claimed on a `message` note; note {id} is a `{}`",
                note.kind
            )));
        }
        let props = note.properties.as_ref().and_then(|v| v.as_object());
        let direction = props
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str());
        if direction != Some("outbound") {
            return Err(RuntimeError::InvalidInput(format!(
                "external_id can only be claimed on an outbound message note; note {id} has \
                 direction {:?}",
                direction
            )));
        }
        let existing = props
            .and_then(|p| p.get("external_id"))
            .and_then(|v| v.as_str());
        if existing.is_some_and(|v| !v.is_empty()) {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} already has an external_id claimed"
            )));
        }
        store
            .set_note_property(
                id,
                "external_id",
                Value::String(external_id),
                note.updated_at,
            )
            .await?;
        store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))
    }

    /// Non-wire outbox scan for the channel delivery loops.
    ///
    /// Pages newest-first through live `message` notes (the same order and
    /// 10k scan cap as the generic `list` verb's filtered offset path) and
    /// returns those with `properties.direction == "outbound"` that are still
    /// pending delivery, capped at `limit`. Pending means `delivered_at` is
    /// absent or null AND `properties.delivery` carries no terminal state
    /// (`"delivered"` / `"failed"`, ADR-122 §1). When `to_prefix` is given,
    /// only rows whose `properties.to_actor` starts with it are counted —
    /// the channel predicate must run BEFORE the limit, otherwise a backlog
    /// of another channel's pending rows starves this channel indefinitely.
    /// This lives on the runtime rather than going through the wire registry
    /// for the same reason as
    /// [`Self::claim_outbound_message_external_id`]: the delivery loop must
    /// scan the backend that actually holds comm's notes, and under a
    /// `[packs.comm]` backend assignment that is not the backend serving the
    /// generic kg verbs.
    pub async fn list_undelivered_outbound_messages(
        &self,
        token: &NamespaceToken,
        to_prefix: Option<&str>,
        limit: u32,
    ) -> RuntimeResult<Vec<khive_storage::note::Note>> {
        const PAGE_SIZE: u32 = 200;
        const MAX_SCAN_TOTAL: u32 = 10_000;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut collected: Vec<khive_storage::note::Note> = Vec::new();
        let mut db_offset: u32 = 0;
        loop {
            let remaining_scan = MAX_SCAN_TOTAL.saturating_sub(db_offset).min(PAGE_SIZE);
            if remaining_scan == 0 {
                break;
            }
            let page = self
                .list_notes(token, Some("message"), remaining_scan, db_offset)
                .await?;
            let fetched = page.len() as u32;
            for note in page {
                if note.deleted_at.is_some() {
                    continue;
                }
                let props = note.properties.as_ref().and_then(|v| v.as_object());
                let outbound = props
                    .and_then(|p| p.get("direction"))
                    .and_then(|v| v.as_str())
                    == Some("outbound");
                if !outbound {
                    continue;
                }
                if let Some(prefix) = to_prefix {
                    let to_matches = props
                        .and_then(|p| p.get("to_actor"))
                        .and_then(|v| v.as_str())
                        .is_some_and(|actor| actor.starts_with(prefix));
                    if !to_matches {
                        continue;
                    }
                }
                // Must match `note_already_delivered` in the delivery loop: a
                // present-but-null `delivered_at` is undelivered, and a
                // terminal `delivery` state ("delivered"/"failed") is not
                // pending even without `delivered_at` (ADR-122 §1).
                let delivered = props
                    .and_then(|p| p.get("delivered_at"))
                    .is_some_and(|v| !v.is_null());
                if delivered {
                    continue;
                }
                let terminal = props
                    .and_then(|p| p.get("delivery"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|state| state == "delivered" || state == "failed");
                if terminal {
                    continue;
                }
                collected.push(note);
                if collected.len() >= limit as usize {
                    return Ok(collected);
                }
            }
            if fetched < PAGE_SIZE {
                break;
            }
            db_offset += fetched;
        }
        Ok(collected)
    }

    /// Assert that `id` names a live outbound `message` note, returning
    /// `InvalidInput` otherwise. Guard shared by the delivery-outcome
    /// markers: they take caller-supplied UUIDs, and the generic
    /// `update_note` they patch through would happily stamp delivery
    /// properties onto any note kind.
    async fn assert_outbound_message(&self, token: &NamespaceToken, id: Uuid) -> RuntimeResult<()> {
        let note = self
            .notes(token)?
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        if note.kind != "message" || note.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} is not a live message note (kind {})",
                note.kind
            )));
        }
        let outbound = note
            .properties
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            == Some("outbound");
        if !outbound {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} is not an outbound message"
            )));
        }
        Ok(())
    }

    /// Mark an outbound `message` note delivered by merging the ADR-122 §1
    /// terminal-outcome properties (`delivery = "delivered"`, `delivered_at`,
    /// and `transport_message_id` when the transport minted one), through the
    /// same patch path the generic `update` verb uses (`delivered_at` is
    /// deliberately not owner-established, pinned by
    /// `generic_update_can_still_patch_delivered_at_on_message_note`).
    /// Refuses (`InvalidInput`) unless `id` names a live outbound `message`
    /// note. Non-wire companion to
    /// [`Self::list_undelivered_outbound_messages`] so the delivery loop
    /// writes the backend that holds the note.
    pub async fn mark_outbound_message_delivered(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        delivered_at: String,
        transport_message_id: Option<String>,
    ) -> RuntimeResult<khive_storage::note::Note> {
        self.assert_outbound_message(token, id).await?;
        let mut props = serde_json::Map::new();
        props.insert("delivery".into(), Value::String("delivered".into()));
        props.insert("delivered_at".into(), Value::String(delivered_at));
        if let Some(transport_message_id) = transport_message_id {
            props.insert(
                "transport_message_id".into(),
                Value::String(transport_message_id),
            );
        }
        self.update_note(
            token,
            id,
            NotePatch {
                properties: Some(Value::Object(props)),
                ..NotePatch::default()
            },
        )
        .await
    }

    /// Record a permanent delivery failure on an outbound `message` note:
    /// `delivery = "failed"`, `failed_at`, `last_error` (ADR-122 §2 — an
    /// allowlist rejection must be recorded, not skipped, or the row stays
    /// pending forever while the caller saw `ok: true`). Refuses
    /// (`InvalidInput`) unless `id` names a live outbound `message` note.
    pub async fn mark_outbound_message_failed(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        failed_at: String,
        last_error: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        self.assert_outbound_message(token, id).await?;
        self.update_note(
            token,
            id,
            NotePatch {
                properties: Some(serde_json::json!({
                    "delivery": "failed",
                    "failed_at": failed_at,
                    "last_error": last_error,
                })),
                ..NotePatch::default()
            },
        )
        .await
    }

    /// Merge `from_id` note into `into_id` note.
    ///
    /// Both notes must exist in the namespace and have the same `kind`. Content is merged
    /// per `content_strategy`. Properties are merged per `strategy`. `from_id` is
    /// tombstoned (status='deleted', deleted_at set). Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating
    /// any rows, edges, or indexes.
    pub async fn merge_note(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_note_with_reason(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            None,
        )
        .await
    }

    /// Merge `from_id` note into `into_id` note and include an optional audit reason.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_note_with_reason(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
    ) -> RuntimeResult<MergeSummary> {
        if let Some(reason) = reason.as_deref() {
            crate::secret_gate::check(reason)?;
        }
        if into_id == from_id {
            return Err(RuntimeError::InvalidInput(
                "cannot merge a note into itself".into(),
            ));
        }
        let ns = token.namespace().as_str().to_string();
        let fts_table = "fts_notes".to_string();
        // Keep deletion, table preparation, and survivor reindex on the same
        // immutable registry view; see the entity merge path above.
        let embedding_plan = EmbeddingModelPlan::capture(self);
        let vec_tables = embedding_plan.vector_tables();
        let pack_rules = self.pack_edge_rules();

        let note_store = self.notes(token)?;
        let into_note = note_store
            .get_note(into_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("not found in this namespace".into()))?;
        Self::ensure_namespace(&into_note.namespace, &ns)?;

        let from_note = note_store
            .get_note(from_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("not found in this namespace".into()))?;
        Self::ensure_namespace(&from_note.namespace, &ns)?;

        reject_pack_managed_schedule_mutation(&into_note, "merge")?;
        reject_pack_managed_schedule_mutation(&from_note, "merge")?;

        let _ = self.graph(token)?;
        let _ = self.text_for_notes(token)?;
        for model_name in embedding_plan.model_names() {
            let _ = self.vectors_for_model(token, model_name)?;
        }

        // Resolved here, where the runtime's installed pack-kind list is in
        // reach; `merge_note_sql` runs on the writer connection with no runtime
        // handle. Both notes share a kind (checked inside), so the into-note's
        // kind decides for the merge.
        let preserve_owner_established = self.is_pack_owned_note_kind(&into_note.kind);

        let pool = self.backend().pool_arc();
        let writer_task = pool.writer_task_handle().ok().flatten();

        let (mut summary, updated_note) = if let Some(writer_task) = writer_task {
            writer_task
                .send(move |conn| {
                    merge_note_sql(
                        conn,
                        ns,
                        fts_table,
                        vec_tables,
                        into_id,
                        from_id,
                        strategy,
                        content_strategy,
                        dry_run,
                        pack_rules,
                        preserve_owner_established,
                        MergeTxLimits::default(),
                    )
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Notes,
                            "merge_note",
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
                    merge_note_sql(
                        conn,
                        ns,
                        fts_table,
                        vec_tables,
                        into_id,
                        from_id,
                        strategy,
                        content_strategy,
                        dry_run,
                        pack_rules,
                        preserve_owner_established,
                        MergeTxLimits::default(),
                    )
                })
            })
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))??
        };

        // Emitted only after the transaction has committed, so the log write
        // never extends the writer hold the budget exists to bound.
        if !dry_run {
            tracing::info!(
                into_id = %summary.kept_id,
                from_id = %summary.removed_id,
                budget_rows = summary.tx_budget.rows_charged,
                budget_bytes = summary.tx_budget.bytes_charged,
                budget_max_rows = summary.tx_budget.max_rows,
                budget_max_bytes = summary.tx_budget.max_bytes,
                "merge_note: transaction materialization budget"
            );
        }

        if !dry_run && !embedding_plan.is_empty() {
            summary.embedding_truncation = self
                .reindex_note_with_plan(token, &updated_note, &embedding_plan)
                .await?;
            // A merge changes the same ANN corpus as update_note's text_changed
            // branch, so fire the same mutation hook regardless of which public
            // write path reached the corpus change.
            self.fire_note_mutation_hook(&updated_note.kind, updated_note.id)
                .await;
        }

        // Dry-run is a read-only preview: it must not append a merge event.
        if !dry_run {
            let event_token =
                token.with_namespace(crate::Namespace::parse(&updated_note.namespace).map_err(
                    |error| RuntimeError::Internal(format!("note namespace invalid: {error}")),
                )?);
            let event_store = self.events(&event_token)?;
            // Mirror the wire-level strategy spelling from MergeParams so consumers
            // can round-trip the policy string back into a request.
            let policy_str = match strategy {
                EntityDedupMergePolicy::PreferInto => "prefer_into",
                EntityDedupMergePolicy::PreferFrom => "prefer_from",
                EntityDedupMergePolicy::Union => "union",
            };
            let mut payload = serde_json::json!({
                "into_id": summary.kept_id,
                "from_id": summary.removed_id,
                "policy": policy_str,
                "content_strategy": format!("{:?}", content_strategy),
                "edges_rewired": summary.edges_rewired,
                "edges_contract_skipped": summary.edges_contract_skipped,
                "edge_conflict_preimages": &summary.edge_conflict_preimages,
            });
            if let Some(reason) = reason {
                payload["reason"] = serde_json::Value::String(reason);
            }
            let event = khive_storage::event::Event::new(
                updated_note.namespace.clone(),
                "merge",
                EventKind::NoteMerged,
                SubstrateKind::Note,
                "",
            )
            .with_target(summary.kept_id)
            .with_payload(payload);
            event_store.append_event(event).await.map_err(|e| {
                RuntimeError::Internal(format!("merge_note: event store write failed: {e}"))
            })?;
        }

        Ok(summary)
    }
}

/// Keep executable schedule intent behind the schedule pack's state-machine verbs.
///
/// `scheduled_event` notes carry both replay payloads and lifecycle state. Allowing
/// generic note update/merge to rewrite either would turn the immutable creator event
/// into a bearer credential for attacker-selected work: replay would attribute the
/// changed row to its original creator. Schedule's own transitions use its private
/// note-store CAS helpers and therefore do not pass through this generic curation seam.
fn reject_pack_managed_schedule_mutation(
    note: &khive_storage::note::Note,
    operation: &str,
) -> RuntimeResult<()> {
    if note.kind == "scheduled_event" {
        return Err(RuntimeError::InvalidInput(format!(
            "cannot {operation} a schedule-managed `scheduled_event` note through generic KG \
             mutation; use schedule.cancel or create a replacement schedule"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FTS document construction
// ---------------------------------------------------------------------------

/// Build the canonical text embedded for an entity on create, update, merge,
/// and repair paths.
pub fn entity_embedding_text(entity: &Entity) -> String {
    match &entity.description {
        Some(description) if !description.is_empty() => {
            format!("{} {description}", entity.name)
        }
        _ => entity.name.clone(),
    }
}

/// Build the canonical text embedded for a note when no explicit bounded
/// embedding prefix was supplied at creation time.
pub fn note_embedding_text(note: &Note) -> String {
    note_embedding_text_ref(note).to_owned()
}

/// Borrow the canonical note embedding text for runtime paths that do not
/// require ownership.
pub(crate) fn note_embedding_text_ref(note: &Note) -> &str {
    &note.content
}

/// Build the `TextDocument` for an entity. This is the single source of truth for
/// entity FTS document shape; all write paths (create, update, merge, reindex, backfill)
/// must go through this function so search parity is guaranteed.
///
/// Body rule: when the entity has a non-empty description, prepend the name
/// (`"<name> <description>"`). Otherwise the body is just the name. This
/// matches the FTS index contract: `title` and `body` are the ranked columns;
/// `tags`, `metadata`, and `namespace` are UNINDEXED.
///
/// `updated_at` is taken from the entity's own timestamp so that backfill and
/// reindex runs record the entity's actual mutation time rather than the
/// reindex execution time.
pub fn entity_fts_document(entity: &Entity) -> TextDocument {
    let updated_at =
        chrono::DateTime::from_timestamp_micros(entity.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: entity.id,
        kind: SubstrateKind::Entity,
        record_kind: Some(entity.kind.clone()),
        title: Some(entity.name.clone()),
        body: entity_embedding_text(entity),
        tags: entity.tags.clone(),
        namespace: entity.namespace.clone(),
        metadata: entity.properties.clone(),
        updated_at,
    }
}

/// Build the `TextDocument` for a note. This is the single source of truth for
/// note FTS document shape; all write paths (create, update, reindex) must go
/// through this function so recall parity is guaranteed. Changes here apply to
/// every caller automatically.
///
/// Body rule: when the note has a `name`, prepend it to the content
/// (`"<name> <content>"`). This matches the FTS index contract: title and body
/// both contribute to ranking, and the name is the most salient signal.
///
/// `updated_at` is taken from the note's own timestamp (not `Utc::now()`) so
/// that backfill and reindex runs record the note's actual mutation time rather
/// than the reindex execution time.
pub fn note_fts_document(note: &Note) -> TextDocument {
    let body = match &note.name {
        Some(n) => format!("{n} {}", note.content),
        None => note.content.clone(),
    };
    let updated_at =
        chrono::DateTime::from_timestamp_micros(note.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: note.id,
        kind: SubstrateKind::Note,
        record_kind: Some(note.kind.clone()),
        title: note.name.clone(),
        body,
        tags: vec![],
        namespace: note.namespace.clone(),
        metadata: note.properties.clone(),
        updated_at,
    }
}

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

/// Derive [`NoteFtsScalars`] from a [`Note`].
///
/// All values match the encoding that [`Fts5TextSearch::upsert_document`]
/// applies when given the output of [`note_fts_document`].
pub(crate) fn note_fts_scalars(note: &Note) -> NoteFtsScalars {
    let doc = note_fts_document(note);
    NoteFtsScalars {
        record_kind: doc.record_kind.unwrap_or_default(),
        title: doc.title.unwrap_or_default(),
        body: doc.body,
        tags: "[]".to_string(),
        metadata: doc
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
        updated_at_micros: doc.updated_at.timestamp_micros(),
    }
}

// ---------------------------------------------------------------------------
// Transactional merge SQL helpers
// ---------------------------------------------------------------------------

/// Cheap SQL-side byte-length probe for one merge entity, evaluated BEFORE
/// [`read_merge_entity`] copies its columns into Rust `String`s and parses
/// `properties`/`tags` as JSON. `LENGTH()` still requires SQLite to touch the
/// stored bytes, but skips the Rust-side allocation and JSON parse — the
/// expensive part for an oversized record. Charging this probe against the
/// budget before the full read means an over-budget record is rejected
/// without ever being materialized or parsed inside the writer transaction.
/// Each column is wrapped in `CAST(... AS BLOB)` — plain `LENGTH(text)`
/// returns SQLite's *character* count for TEXT values, not the UTF-8 byte
/// count the budget is denominated in, so a multibyte (CJK/emoji) record
/// could under-report and pass a probe its true byte size exceeds. Casting
/// to BLOB forces `LENGTH()` to report octets instead.
/// A missing row probes as zero; `read_merge_entity`'s own "not found" error
/// fires on the subsequent full read and is unaffected by this probe.
fn probe_merge_entity_bytes(conn: &rusqlite::Connection, id: Uuid) -> Result<usize, SqliteError> {
    let id_str = id.to_string();
    let len: Option<i64> = conn
        .query_row(
            "SELECT LENGTH(CAST(name AS BLOB)) \
                    + COALESCE(LENGTH(CAST(description AS BLOB)), 0) \
                    + COALESCE(LENGTH(CAST(properties AS BLOB)), 0) \
                    + LENGTH(CAST(tags AS BLOB)) \
             FROM entities WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id_str],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;
    Ok(128_usize.saturating_add(len.unwrap_or(0).max(0) as usize))
}

/// Read one entity row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_entity(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<Entity, SqliteError> {
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
         created_at, updated_at, deleted_at, merged_into, merge_event_id, \
         (SELECT a.content_ref FROM attachments AS a \
          WHERE a.record_uuid = entities.id AND a.substrate = 'entity' \
            AND a.role = 'content') AS content_ref \
         FROM entities WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("entity {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let entity_type: Option<String> = row.get(3)?;
    let name: String = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let properties_str: Option<String> = row.get(6)?;
    let tags_str: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let deleted_at: Option<i64> = row.get(10)?;
    let merged_into_str: Option<String> = row.get(11)?;
    let merge_event_id_str: Option<String> = row.get(12)?;
    let content_ref: Option<String> = row.get(13)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "entity {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let entity_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<Value> = properties_str
        .map(|s| {
            serde_json::from_str::<Value>(&s).map_err(|e| SqliteError::InvalidData(e.to_string()))
        })
        .transpose()?;
    let tags: Vec<String> =
        serde_json::from_str(&tags_str).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merged_into = merged_into_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merge_event_id = merge_event_id_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;

    Ok(Entity {
        id: entity_id,
        namespace: ns,
        kind,
        entity_type,
        name,
        description,
        properties,
        tags,
        created_at,
        updated_at,
        deleted_at,
        merged_into,
        merge_event_id,
        content_ref,
    })
}

/// All merge SQL on one connection inside an already-open `BEGIN IMMEDIATE` transaction.
///
/// Reads both entities, rewires/drops incident edges, merges entity fields, updates FTS,
/// deletes the `from` vec entry (if `vec_table` is Some), and tombstones `from` with merge
/// provenance.  Returns the updated `into` entity so the caller can do the async vec re-insert.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: merge requires both entity IDs, the namespace, FTS and vec table names, merge
// policy, and dry-run flag — all are load-bearing; reducing to a struct would obscure
// the sync/async boundary split that keeps this function off the async runtime.
#[allow(clippy::too_many_arguments)]
fn merge_entity_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
    pack_rules: Vec<EdgeEndpointRule>,
    validation: EntityMergeValidation,
    limits: MergeTxLimits,
) -> Result<(MergeSummary, Entity), MergeEntitySqlError> {
    let mut budget = MergeTxBudget::new(limits);
    // Config-scaled fanout (one FTS/vector delete per table, one contract rule
    // set per pack) is charged in bytes only: it is bounded by configuration,
    // not by graph shape, but belongs in the same account it amortizes over.
    budget.charge(
        0,
        vec_tables.iter().map(String::len).sum::<usize>()
            + pack_rules.len() * std::mem::size_of::<EdgeEndpointRule>(),
        "preparing pack and vector fanout",
    )?;

    budget.charge(
        1,
        probe_merge_entity_bytes(conn, into_id)?,
        "reading merge records",
    )?;
    let into_entity = read_merge_entity(conn, into_id, &namespace)?;
    budget.charge(
        1,
        probe_merge_entity_bytes(conn, from_id)?,
        "reading merge records",
    )?;
    let from_entity = read_merge_entity(conn, from_id, &namespace)?;

    match validation {
        EntityMergeValidation::LegacyKind if into_entity.kind != from_entity.kind => {
            return Err(MergeEntitySqlError::Refusal(
                EntityMergeRefusal::LegacyKind {
                    into_id,
                    into_kind: into_entity.kind,
                    from_id,
                    from_kind: from_entity.kind,
                },
            ));
        }
        EntityMergeValidation::SafetyFloor => {
            validate_entity_merge_floor(&into_entity, &from_entity).map_err(|guard| {
                MergeEntitySqlError::Refusal(EntityMergeRefusal::SafetyFloor(guard))
            })?;
        }
        EntityMergeValidation::LegacyKind | EntityMergeValidation::Forced => {}
    }

    // --- Collect edges incident to from_id ---
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    let from_str = from_id.to_string();

    // Namespace-agnostic (khive#1236): edge endpoints resolve by-ID regardless of
    // namespace (ADR-007 Rev 6), and `link` stamps an edge with its *creator's*
    // namespace, not either endpoint's — so an edge incident to `from_id` can live
    // in any namespace. Scoping this collection to the merge's own namespace missed
    // those edges entirely. Each row's own `namespace` column is carried through
    // (`EdgeRow::namespace`) and used for every subsequent SQL op against that row.
    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            outbound.push(edge);
        }
    }

    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE target_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            inbound.push(edge);
        }
    }

    // Deduplicate by edge ID (a self-edge from_id→from_id appears in both lists).
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }
    let original_edges: HashMap<Uuid, EdgeRow> = all_edges
        .iter()
        .map(|edge| (edge.id, edge.clone()))
        .collect();

    // --- Merge entity fields ---
    let (merged_props, properties_merged) =
        merge_properties(&into_entity.properties, &from_entity.properties, strategy);
    let merged_name = merge_string_field(&into_entity.name, &from_entity.name, strategy);
    let (merged_description, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            let into_desc = into_entity.description.as_deref().unwrap_or("");
            let from_desc = from_entity.description.as_deref().unwrap_or("");
            if from_desc.is_empty() {
                (into_entity.description.clone(), false)
            } else if into_desc.is_empty() {
                (from_entity.description.clone(), true)
            } else {
                (Some(format!("{}\n\n---\n\n{}", into_desc, from_desc)), true)
            }
        }
        // Description selection follows `content_strategy` directly — it is a
        // deliberate, independently-settable choice, not derived from the
        // entity-field `strategy` (properties/name/tags merge policy).
        ContentMergeStrategy::PreferInto => (into_entity.description.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_entity.description.clone(), false),
    };
    let (merged_tags, tags_unioned) = union_tags(&into_entity.tags, &from_entity.tags);

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let tags_json = serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string());

    // Writes are gated on `!dry_run` below, but the loop itself always runs so a
    // dry-run response reports a predictive `edges_rewired` count instead of zero.
    let mut rewired_edge_ids = HashSet::new();
    let mut edges_contract_skipped = 0usize;
    let mut edge_conflict_preimages = Vec::new();
    let mut conflict_deleted_edge_ids = HashSet::new();
    for edge in all_edges {
        if conflict_deleted_edge_ids.contains(&edge.id) {
            continue;
        }
        let raw_src = if edge.source_id == from_id {
            into_id
        } else {
            edge.source_id
        };
        let raw_tgt = if edge.target_id == from_id {
            into_id
        } else {
            edge.target_id
        };
        let relation_typed = edge.relation.parse::<EdgeRelation>().ok();
        // Symmetric relations must be stored with source_uuid < target_uuid.
        // Apply canonicalization so the conflict check and UPDATE both use the canonical form.
        let (new_src, new_tgt) = match relation_typed {
            Some(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
            None => (raw_src, raw_tgt),
        };

        if new_src == new_tgt {
            if !dry_run {
                conn.execute(
                    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                    rusqlite::params![&edge.namespace, edge.id.to_string()],
                )?;
            }
            continue;
        }

        // Endpoint-contract check (khive#1216): the rewired triple must still pass
        // the same allowlist `link` enforces. `into_id` and `from_id` share `kind`
        // (enforced by the caller), but `entity_type` may differ between them, so a
        // pack rule scoped via `EntityOfType` can accept `from_id`'s edge yet reject
        // the post-rewrite pair against `into_id`. A violating edge is dropped and
        // counted, mirroring the existing dangling-endpoint skip behavior rather
        // than silently writing a contract-violating edge or aborting the merge.
        let contract_ok = match relation_typed {
            // `annotates` targets may be events or edges, which
            // `resolve_merge_edge_endpoint` cannot resolve — evaluate its
            // (unconditional) exemption before endpoint resolution so valid
            // annotates edges are not dropped as unresolvable.
            Some(EdgeRelation::Annotates) => true,
            Some(rel) => {
                let src_info = if new_src == into_id {
                    Some((
                        "entity",
                        into_entity.kind.clone(),
                        into_entity.entity_type.clone(),
                    ))
                } else {
                    resolve_merge_edge_endpoint_budgeted(conn, new_src, &mut budget)?
                };
                let tgt_info = if new_tgt == into_id {
                    Some((
                        "entity",
                        into_entity.kind.clone(),
                        into_entity.entity_type.clone(),
                    ))
                } else {
                    resolve_merge_edge_endpoint_budgeted(conn, new_tgt, &mut budget)?
                };
                match (src_info, tgt_info) {
                    (Some((src_sub, src_kind, src_type)), Some((tgt_sub, tgt_kind, tgt_type))) => {
                        merge_rewire_endpoint_contract_allows(
                            &pack_rules,
                            rel,
                            src_sub,
                            &src_kind,
                            src_type.as_deref(),
                            tgt_sub,
                            &tgt_kind,
                            tgt_type.as_deref(),
                        )
                    }
                    // An endpoint no longer resolves (e.g. concurrently hard-deleted)
                    // — cannot evaluate the contract, so drop rather than assume ok.
                    _ => false,
                }
            }
            // Relation string predates the closed EdgeRelation enum (pre-migration
            // data); leave existing behavior in place rather than guessing.
            None => true,
        };
        if !contract_ok {
            if !dry_run {
                conn.execute(
                    "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                    rusqlite::params![&edge.namespace, edge.id.to_string()],
                )?;
            }
            tracing::warn!(
                edge_id = %edge.id,
                source = %new_src,
                target = %new_tgt,
                relation = %edge.relation,
                "merge_entity: dropping rewired edge — endpoint contract violation post-merge"
            );
            edges_contract_skipped += 1;
            continue;
        }

        let now_ts = chrono::Utc::now().timestamp_micros();
        // Preserve the original edge ID where possible so callers can still get()
        // it by the ID returned from link(): update in-place when there's no
        // conflict; when into_id already owns this (source,target,relation), the
        // incoming (from-side) duplicate is dropped and the existing into-edge is
        // left untouched (ADR-039 `ON CONFLICT ... DO NOTHING` semantics).
        // Check for a conflict: does into_id already have this natural key?
        let conflict_id: Option<String> = {
            let conflict_src = new_src.to_string();
            let conflict_tgt = new_tgt.to_string();
            conn.query_row(
                khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                rusqlite::params![
                    &edge.namespace,
                    &conflict_src,
                    &conflict_tgt,
                    &edge.relation,
                    edge.id.to_string(),
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(SqliteError::Rusqlite)?
        };

        if let Some(conflict_id) = conflict_id {
            // A live or soft-deleted row already owns this natural key: drop the
            // incoming duplicate. The surviving row's weight/metadata/deleted_at
            // are never mutated or resurrected. Capture the duplicate and the
            // complete hard-delete cascade before removing either, so the audit
            // event contains enough state to restore every destroyed row.
            let surviving_edge_id = Uuid::parse_str(&conflict_id)
                .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
            let incident_edge_preimages = collect_conflict_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &mut budget,
            )?;
            for incident in &incident_edge_preimages {
                conflict_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            conflict_deleted_edge_ids.insert(edge.id);
            rewired_edge_ids.insert(edge.id);

            if !dry_run {
                delete_conflict_incident_edges(conn, &incident_edge_preimages)?;
                conn.execute(
                    khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
                    rusqlite::params![&edge.namespace, edge.id.to_string()],
                )?;
            }
            edge_conflict_preimages.push(MergeEdgeConflictPreimage {
                surviving_edge_id,
                dropped_edge: edge_row_preimage(&edge)?,
                incident_edge_preimages,
            });
        } else {
            if dry_run {
                rewired_edge_ids.insert(edge.id);
                continue;
            }
            let changed = conn.execute(
                "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                rusqlite::params![
                    new_src.to_string(),
                    new_tgt.to_string(),
                    now_ts,
                    &edge.namespace,
                    edge.id.to_string(),
                ],
            )?;
            if changed > 0 {
                rewired_edge_ids.insert(edge.id);
            }
        }
    }
    let edges_rewired = rewired_edge_ids.len();

    if !dry_run {
        // UPDATE only the merged fields — a full-row INSERT OR REPLACE silently
        // nulls any column missing from its list (entity_type and the former
        // entity-owned content_ref were lost this way; khive#1214). Attachments
        // now live in their own table and this targeted UPDATE leaves them alone.
        conn.execute(
            "UPDATE entities SET \
                 name = ?1, description = ?2, properties = ?3, tags = ?4, \
                 updated_at = ?5, merged_into = NULL, merge_event_id = NULL \
             WHERE namespace = ?6 AND id = ?7",
            rusqlite::params![
                &merged_name,
                &merged_description,
                &props_str,
                &tags_json,
                now,
                &namespace,
                &into_str,
            ],
        )?;

        // Body formula mirrors entity_fts_document (the canonical constructor):
        // this path is sync/spawn_blocking so it can't call it directly, but
        // must stay field-identical.
        let fts_body = match &merged_description {
            Some(d) if !d.is_empty() => format!("{} {}", merged_name, d),
            _ => merged_name.clone(),
        };
        let kind_str = SubstrateKind::Entity.to_string();
        let fts_map = khive_db::stores::text::rowid_map_table(&fts_table);

        // `into`'s old FTS row (via the map, not a namespace/subject_id
        // scan), then the new merged row, then the map upsert to the new
        // rowid. No separate map-row delete first: `INSERT OR REPLACE`
        // overwrites it in place (see `delete_document_statement`'s doc
        // comment in khive-db).
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2)"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {} \
                (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                &kind_str,
                &merged_name,
                &fts_body,
                &tags_json,
                &namespace,
                &props_str,
                now,
                &into_entity.kind,
            ],
        )?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO {fts_map} (namespace, subject_id, rowid) \
                 VALUES (?1, ?2, last_insert_rowid())"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;

        // `from`'s FTS row is gone for good (merged away, not reinserted) —
        // its map row must be removed too, or it would keep pointing at a
        // rowid the DELETE above already reclaimed.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2)"
            ),
            rusqlite::params![&namespace, &from_str],
        )?;
        conn.execute(
            &format!("DELETE FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![&namespace, &from_str],
        )?;

        khive_db::stores::vectors::delete_subject_from_vector_tables(
            conn,
            &vec_tables,
            from_id,
            &namespace,
        )?;

        let merge_event_id = Uuid::new_v4();
        conn.execute(
            "UPDATE entities \
             SET deleted_at = ?1, merged_into = ?2, merge_event_id = ?3, updated_at = ?1 \
             WHERE namespace = ?4 AND id = ?5 AND deleted_at IS NULL",
            rusqlite::params![
                now,
                into_str,
                merge_event_id.to_string(),
                &namespace,
                &from_str,
            ],
        )?;
    }

    let updated_entity = Entity {
        id: into_id,
        namespace,
        kind: into_entity.kind,
        entity_type: into_entity.entity_type,
        name: merged_name,
        description: merged_description,
        properties: merged_props,
        tags: merged_tags,
        created_at: into_entity.created_at,
        updated_at: now,
        deleted_at: into_entity.deleted_at,
        merged_into: None,
        merge_event_id: None,
        content_ref: into_entity.content_ref,
    };

    Ok((
        MergeSummary {
            kept_id: into_id,
            removed_id: from_id,
            edges_rewired,
            edges_contract_skipped,
            edge_conflict_preimages,
            properties_merged,
            tags_unioned,
            content_appended,
            dry_run,
            tx_budget: budget.report(),
            embedding_truncation: Default::default(),
        },
        updated_entity,
    ))
}

// ---------------------------------------------------------------------------
// Note merge SQL helpers
// ---------------------------------------------------------------------------

/// Cheap SQL-side byte-length probe for one merge note — see
/// [`probe_merge_entity_bytes`] for why this runs before
/// [`read_merge_note`]'s full column copy and JSON parse, and why each
/// column is cast to BLOB before `LENGTH()`.
fn probe_merge_note_bytes(conn: &rusqlite::Connection, id: Uuid) -> Result<usize, SqliteError> {
    let id_str = id.to_string();
    let len: Option<i64> = conn
        .query_row(
            "SELECT COALESCE(LENGTH(CAST(name AS BLOB)), 0) \
                    + LENGTH(CAST(content AS BLOB)) \
                    + COALESCE(LENGTH(CAST(properties AS BLOB)), 0) \
             FROM notes WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id_str],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;
    Ok(128_usize.saturating_add(len.unwrap_or(0).max(0) as usize))
}

/// Read one note row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_note(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<khive_storage::note::Note, SqliteError> {
    use khive_storage::note::Note;
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
         expires_at, properties, created_at, updated_at, deleted_at \
         FROM notes WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("note {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    let name: Option<String> = row.get(4)?;
    let content: String = row.get(5)?;
    let salience: Option<f64> = row.get(6)?;
    let decay_factor: Option<f64> = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    let properties_str: Option<String> = row.get(9)?;
    let created_at: i64 = row.get(10)?;
    let updated_at: i64 = row.get(11)?;
    let deleted_at: Option<i64> = row.get(12)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "note {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let note_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<serde_json::Value> = properties_str
        .map(|s| serde_json::from_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string())))
        .transpose()?;

    Ok(Note {
        id: note_id,
        namespace: ns,
        kind,
        status,
        name,
        content,
        salience,
        decay_factor,
        expires_at,
        properties,
        created_at,
        updated_at,
        deleted_at,
    })
}

fn max_option_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn append_merge_history(props: Option<Value>, entry: Value) -> Result<Option<Value>, SqliteError> {
    use serde_json::{json, Map};
    let mut obj: Map<String, Value> = match props {
        Some(Value::Object(m)) => m,
        Some(other) => {
            let mut m = Map::new();
            m.insert("_value".into(), other);
            m
        }
        None => Map::new(),
    };
    let history = obj
        .entry("_merge_history".to_string())
        .or_insert_with(|| json!([]));
    if let Value::Array(arr) = history {
        arr.push(entry);
    }
    Ok(Some(Value::Object(obj)))
}

/// All note merge SQL on one connection inside a `BEGIN IMMEDIATE` transaction.
///
/// Reads both notes (must have same `kind`), rewires/drops incident edges, merges content
/// per `content_strategy`, tombstones `from`. Returns the updated `into` note for async
/// re-embedding.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: note merge additionally requires a content_strategy parameter versus entity merge;
// same sync/async boundary rationale as merge_entity_sql applies here.
#[allow(clippy::too_many_arguments)]
fn merge_note_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
    pack_rules: Vec<EdgeEndpointRule>,
    preserve_owner_established: bool,
    limits: MergeTxLimits,
) -> Result<(MergeSummary, khive_storage::note::Note), SqliteError> {
    let mut budget = MergeTxBudget::new(limits);
    // Same accounting as `merge_entity_sql`: config-scaled fanout in bytes only.
    budget.charge(
        0,
        vec_tables.iter().map(String::len).sum::<usize>()
            + pack_rules.len() * std::mem::size_of::<EdgeEndpointRule>(),
        "preparing pack and vector fanout",
    )?;

    budget.charge(
        1,
        probe_merge_note_bytes(conn, into_id)?,
        "reading merge records",
    )?;
    let into_note = read_merge_note(conn, into_id, &namespace)?;
    budget.charge(
        1,
        probe_merge_note_bytes(conn, from_id)?,
        "reading merge records",
    )?;
    let from_note = read_merge_note(conn, from_id, &namespace)?;

    if into_note.kind != from_note.kind {
        return Err(SqliteError::InvalidData(format!(
            "cannot merge notes of different kinds: {} vs {}",
            into_note.kind, from_note.kind
        )));
    }

    // A quarantined message participates in no merges, in either role. Folding
    // its content into an ordinary message would retain the body while the
    // marker restoration below drops the `quarantined` disposition — laundering
    // quarantined transport content into an unmarked record. Release is the
    // channel-ingest path's decision, never a side effect of curation.
    if into_note.kind == "message"
        && (message_is_quarantined(&into_note) || message_is_quarantined(&from_note))
    {
        return Err(SqliteError::InvalidData(
            "cannot merge a quarantined message: quarantine disposition is              transport-owned and must be released by the channel-ingest path              before the content can be folded into another record"
                .to_string(),
        ));
    }

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let from_str = from_id.to_string();

    // Collect edges incident to from_id.
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    // Namespace-agnostic (khive#1236): see the equivalent comment in
    // `merge_entity_sql` — edge endpoints resolve by-ID regardless of namespace.
    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            outbound.push(edge);
        }
    }
    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE target_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            inbound.push(edge);
        }
    }
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }
    let original_edges: HashMap<Uuid, EdgeRow> = all_edges
        .iter()
        .map(|edge| (edge.id, edge.clone()))
        .collect();

    // Merge note fields.
    let (merged_content, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            if from_note.content.is_empty() {
                (into_note.content.clone(), false)
            } else {
                (
                    format!("{}\n\n---\n\n{}", into_note.content, from_note.content),
                    true,
                )
            }
        }
        ContentMergeStrategy::PreferInto => (into_note.content.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_note.content.clone(), false),
    };

    let merged_name = match strategy {
        EntityDedupMergePolicy::PreferFrom => from_note.name.clone().or(into_note.name.clone()),
        _ => into_note.name.clone().or(from_note.name.clone()),
    };

    let (mut merged_props, _) =
        merge_properties(&into_note.properties, &from_note.properties, strategy);

    // A merge folds two records together; it does not transfer attribution.
    // On a pack-owned note kind the into-note's owned identity properties are
    // restored after the fold, under every strategy including `PreferFrom`, so
    // the surviving row still says who wrote it.
    if preserve_owner_established {
        preserve_owner_established_properties(&into_note.properties, &mut merged_props);
    }
    if into_note.kind == "message" {
        preserve_message_transport_properties(&into_note.properties, &mut merged_props);
    }

    // Recomputed from the final retained properties rather than carried
    // forward from the fold's own count. The fold's count and post-
    // restoration reality diverge whenever an owner-established key holds a
    // nested object: `union` recurses into it and counts the absorbed
    // note's leaf as merged, but restoration then reverts the whole key,
    // and the fold's flat "keys contributed" number cannot express a
    // partial reversal of a nested contribution. Diffing the final object
    // against the into-note's pre-merge properties sidesteps that fold/
    // restoration coupling entirely.
    let properties_merged = count_new_property_keys(
        into_note.properties.as_ref(),
        merged_props.as_ref(),
        strategy,
    );

    let merge_history_entry = serde_json::json!({
        "merged_from": from_id.to_string(),
        "merged_at": now,
        "strategy": format!("{:?}", strategy),
        "content_strategy": format!("{:?}", content_strategy),
    });
    let merged_props = append_merge_history(merged_props, merge_history_entry)?;

    let merged_salience = max_option_f64(into_note.salience, from_note.salience);
    let merged_expires_at = match (into_note.expires_at, from_note.expires_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    // The loop always runs so a dry-run reports a predictive `edges_rewired`
    // count instead of zero (mirrors the entity merge path).
    let mut rewired_edge_ids = HashSet::new();
    let mut edges_contract_skipped = 0usize;
    let mut edge_conflict_preimages = Vec::new();
    let mut conflict_deleted_edge_ids = HashSet::new();
    {
        for edge in all_edges {
            if conflict_deleted_edge_ids.contains(&edge.id) {
                continue;
            }
            let raw_src = if edge.source_id == from_id {
                into_id
            } else {
                edge.source_id
            };
            let raw_tgt = if edge.target_id == from_id {
                into_id
            } else {
                edge.target_id
            };
            let relation_typed = edge.relation.parse::<EdgeRelation>().ok();
            // Canonicalize symmetric relations before conflict check + UPDATE.
            let (new_src, new_tgt) = match relation_typed {
                Some(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
                None => (raw_src, raw_tgt),
            };
            if new_src == new_tgt {
                if !dry_run {
                    conn.execute(
                        "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                        rusqlite::params![&edge.namespace, edge.id.to_string()],
                    )?;
                }
                continue;
            }

            // Endpoint-contract check (khive#1216/#1236): see the equivalent
            // block in `merge_entity_sql` for the full rationale. Here the
            // rewiring endpoint is a note (`into_id`'s kind, substrate "note"),
            // not an entity.
            let contract_ok = match relation_typed {
                // Same rationale as the entity-merge path: annotates targets may
                // be events or edges, unresolvable by substrate lookup — the
                // exemption must precede endpoint resolution.
                Some(EdgeRelation::Annotates) => true,
                Some(rel) => {
                    let src_info = if new_src == into_id {
                        Some(("note", into_note.kind.clone(), None))
                    } else {
                        resolve_merge_edge_endpoint_budgeted(conn, new_src, &mut budget)?
                    };
                    let tgt_info = if new_tgt == into_id {
                        Some(("note", into_note.kind.clone(), None))
                    } else {
                        resolve_merge_edge_endpoint_budgeted(conn, new_tgt, &mut budget)?
                    };
                    match (src_info, tgt_info) {
                        (
                            Some((src_sub, src_kind, src_type)),
                            Some((tgt_sub, tgt_kind, tgt_type)),
                        ) => merge_rewire_endpoint_contract_allows(
                            &pack_rules,
                            rel,
                            src_sub,
                            &src_kind,
                            src_type.as_deref(),
                            tgt_sub,
                            &tgt_kind,
                            tgt_type.as_deref(),
                        ),
                        _ => false,
                    }
                }
                None => true,
            };
            if !contract_ok {
                if !dry_run {
                    conn.execute(
                        "DELETE FROM graph_edges WHERE namespace = ?1 AND id = ?2",
                        rusqlite::params![&edge.namespace, edge.id.to_string()],
                    )?;
                }
                tracing::warn!(
                    edge_id = %edge.id,
                    source = %new_src,
                    target = %new_tgt,
                    relation = %edge.relation,
                    "merge_note: dropping rewired edge — endpoint contract violation post-merge"
                );
                edges_contract_skipped += 1;
                continue;
            }

            let now_ts = chrono::Utc::now().timestamp_micros();
            let conflict_id: Option<String> = {
                let conflict_src = new_src.to_string();
                let conflict_tgt = new_tgt.to_string();
                conn.query_row(
                    khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                    rusqlite::params![
                        &edge.namespace,
                        &conflict_src,
                        &conflict_tgt,
                        &edge.relation,
                        edge.id.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(SqliteError::Rusqlite)?
            };

            if let Some(conflict_id) = conflict_id {
                // A live or soft-deleted row already owns this natural key: drop
                // the incoming duplicate (ADR-039 `ON CONFLICT ... DO NOTHING`).
                // The surviving row's weight/metadata/deleted_at are never
                // mutated or resurrected. Match hard `delete_edge`: cascade
                // incident annotations, and preserve every removed row first.
                let surviving_edge_id = Uuid::parse_str(&conflict_id)
                    .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
                let incident_edge_preimages = collect_conflict_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &mut budget,
                )?;
                for incident in &incident_edge_preimages {
                    conflict_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                conflict_deleted_edge_ids.insert(edge.id);
                rewired_edge_ids.insert(edge.id);

                if !dry_run {
                    delete_conflict_incident_edges(conn, &incident_edge_preimages)?;
                    conn.execute(
                        khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
                        rusqlite::params![&edge.namespace, edge.id.to_string()],
                    )?;
                }
                edge_conflict_preimages.push(MergeEdgeConflictPreimage {
                    surviving_edge_id,
                    dropped_edge: edge_row_preimage(&edge)?,
                    incident_edge_preimages,
                });
            } else {
                if dry_run {
                    rewired_edge_ids.insert(edge.id);
                    continue;
                }
                let changed = conn.execute(
                    "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                    rusqlite::params![
                        new_src.to_string(),
                        new_tgt.to_string(),
                        now_ts,
                        &edge.namespace,
                        edge.id.to_string(),
                    ],
                )?;
                if changed > 0 {
                    rewired_edge_ids.insert(edge.id);
                }
            }
        }
    }
    let edges_rewired = rewired_edge_ids.len();

    if !dry_run {
        conn.prepare_cached(khive_db::stores::note::NOTE_UPSERT_SQL)?
            .execute(rusqlite::params![
                &into_str,
                &namespace,
                &into_note.kind,
                &into_note.status,
                &merged_name,
                &merged_content,
                merged_salience,
                into_note.decay_factor,
                merged_expires_at,
                &props_str,
                into_note.created_at,
                now,
                into_note.deleted_at,
            ])?;

        let fts_map = khive_db::stores::text::rowid_map_table(&fts_table);

        // `into`'s old FTS row (via the map), then the new merged row, then
        // the map upsert to the new rowid — see `merge_entity_sql`'s matching
        // comment for why no separate map-row delete is needed here.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2)"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        // Derive FTS scalars through the shared constructor so this raw SQL path
        // is field-identical to TextSearch::upsert_document: critically, `title`
        // is an empty string (not SQL NULL) for nameless notes, so get_document
        // round-trips None <-> "" correctly.
        let fts_merged = {
            let mut merged_note = Note::new(&namespace, &*into_note.kind, &*merged_content);
            merged_note.id = into_id;
            merged_note.name = merged_name.clone();
            merged_note.properties = merged_props.clone();
            merged_note.updated_at = now;
            note_fts_scalars(&merged_note)
        };
        conn.execute(
            &format!(
                "INSERT INTO {} \
                (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                SubstrateKind::Note.to_string(),
                &fts_merged.title,
                &fts_merged.body,
                &fts_merged.tags,
                &namespace,
                &fts_merged.metadata,
                fts_merged.updated_at_micros,
                &fts_merged.record_kind,
            ],
        )?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO {fts_map} (namespace, subject_id, rowid) \
                 VALUES (?1, ?2, last_insert_rowid())"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;

        // `from`'s FTS row is gone for good — remove its map row too.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2)"
            ),
            rusqlite::params![&namespace, &from_str],
        )?;
        conn.execute(
            &format!("DELETE FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![&namespace, &from_str],
        )?;

        khive_db::stores::vectors::delete_subject_from_vector_tables(
            conn,
            &vec_tables,
            from_id,
            &namespace,
        )?;

        conn.execute(
            "UPDATE notes SET status = 'deleted', deleted_at = ?1, updated_at = ?1 \
             WHERE namespace = ?2 AND id = ?3 AND deleted_at IS NULL",
            rusqlite::params![now, &namespace, &from_str],
        )?;
    }

    let updated_note = khive_storage::note::Note {
        id: into_id,
        namespace: namespace.clone(),
        kind: into_note.kind.clone(),
        status: into_note.status.clone(),
        name: merged_name,
        content: merged_content,
        salience: merged_salience,
        decay_factor: into_note.decay_factor,
        expires_at: merged_expires_at,
        properties: merged_props,
        created_at: into_note.created_at,
        updated_at: now,
        deleted_at: into_note.deleted_at,
    };

    Ok((
        MergeSummary {
            kept_id: into_id,
            removed_id: from_id,
            edges_rewired,
            edges_contract_skipped,
            edge_conflict_preimages,
            properties_merged,
            tags_unioned: 0,
            content_appended,
            dry_run,
            tx_budget: budget.report(),
            embedding_truncation: Default::default(),
        },
        updated_note,
    ))
}

// ---------------------------------------------------------------------------
// Merge helpers (pure functions — easier to unit test)
// ---------------------------------------------------------------------------

/// `pub(crate)` so `crate::atomic_prepare::prepare_merge` can reuse this exact
/// field-fold semantics for atomic/non-atomic parity.
pub(crate) fn merge_string_field(
    into: &str,
    from: &str,
    strategy: EntityDedupMergePolicy,
) -> String {
    match strategy {
        EntityDedupMergePolicy::PreferInto | EntityDedupMergePolicy::Union => into.to_string(),
        EntityDedupMergePolicy::PreferFrom => from.to_string(),
    }
}

/// Property keys on a pack-owned note that the owning pack establishes and
/// then reads back to decide something structural about the record.
///
/// The test for membership is that both halves hold: the key is written
/// under the owner's authority rather than from caller input, AND its value
/// is read to decide identity, grouping, routing, lifecycle, visibility,
/// authorization, deduplication, or membership. `from_actor`, `direction` and
/// `sent_at` answer "who wrote this, in which direction, when"; `outbound_ref`
/// and `thread_id` answer "which record is this one's author-side original,
/// and which conversation does it belong to". `subject` is reproduced
/// verbatim when a record is re-emitted; `wire_message_id` and `external_id`
/// are the author-side citation and correlation key a reply is routed
/// against. The set is therefore not "keys that identify a party" — it is
/// "keys the owner established and later trusts".
///
/// Naming one of these in a caller-supplied `properties` patch is refused by
/// `update` on a pack-owned kind (see [`owner_established_property_named_in`]).
///
/// `to_actor` belongs here alongside `from_actor`: comm establishes it at
/// send time from the `to=` param, and `comm.read` trusts a present string
/// value to decide whether the caller is the addressee, failing open only
/// when the key is absent or non-string. A caller must not be able to
/// retarget a delivered message's addressee via a patch that names no other
/// currently-protected key.
///
/// Membership here governs writes to an EXISTING record only. Introducing one
/// of these keys at create time is a separate question and is not addressed
/// by this constant.
pub(crate) const OWNER_ESTABLISHED_PROPERTIES: &[&str] = &[
    "from_actor",
    "to_actor",
    "direction",
    "sent_at",
    "outbound_ref",
    "thread_id",
    "subject",
    "wire_message_id",
    "external_id",
];

/// Transport evidence that only `comm.ingest` may establish on a `message`.
///
/// This list is deliberately separate from [`OWNER_ESTABLISHED_PROPERTIES`].
/// The latter applies to every pack-owned note kind; quarantine disposition
/// and channel attribution are message-only, and protecting these names on a
/// task, memory, or another pack-owned note would reserve ordinary metadata
/// outside ADR-056's scope.
pub(crate) const MESSAGE_TRANSPORT_OWNED_PROPERTIES: &[&str] =
    &["quarantined", "channel_kind", "channel_slug"];

/// Whether a stored message note carries a live quarantine disposition.
///
/// The marker is written by transports as JSON `true` and by some channel
/// adapters as the string `"true"`; both spellings are live in stored data
/// (`comm.health` counts both). Any present value other than an explicit
/// boolean `false` or string `"false"` reads as quarantined, so an unexpected
/// encoding fails closed.
fn message_is_quarantined(note: &khive_storage::note::Note) -> bool {
    let Some(Value::Object(map)) = note.properties.as_ref() else {
        return false;
    };
    match map.get("quarantined") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => value != "false",
        Some(_) => true,
    }
}

fn message_transport_owned_property_named_in(patch: &Value) -> Option<&'static str> {
    let Value::Object(map) = patch else {
        return None;
    };
    MESSAGE_TRANSPORT_OWNED_PROPERTIES
        .iter()
        .copied()
        .find(|key| map.contains_key(*key))
}

/// The first [`OWNER_ESTABLISHED_PROPERTIES`] key a caller-supplied
/// `properties` patch names, if any.
///
/// Naming a key is the whole test: `update_note` folds the patch with
/// `PreferFrom`, so a named key overwrites the stored value and an unnamed one
/// leaves it untouched. A non-object patch names nothing.
pub(crate) fn owner_established_property_named_in(patch: &Value) -> Option<&'static str> {
    let Value::Object(map) = patch else {
        return None;
    };
    OWNER_ESTABLISHED_PROPERTIES
        .iter()
        .copied()
        .find(|key| map.contains_key(*key))
}

/// Restore the into-note's [`OWNER_ESTABLISHED_PROPERTIES`] into `merged`
/// after a property fold.
///
/// A key absent on the into-note is removed from `merged` rather than left as
/// the from-note's value: a record that carried no owner-established value
/// must not acquire one by being merged into. That applies to grouping as much
/// as to attribution — a note with no `thread_id` must not join a conversation
/// because another note was folded into it.
///
/// A fold can also yield a value that is not an object at all: `merge_json`
/// applies a non-object `from` directly under `PreferFrom`, replacing the
/// into-note's whole object with a scalar. A scalar cannot carry the
/// owner-established keys, so there is nothing to restore them into and they
/// would be erased. The into-note's properties are kept instead — the scalar
/// contributes no key that could coexist with them, so nothing the fold
/// intended is lost.
///
/// This function only restores values; callers that need to report how many
/// properties genuinely survived a merge should diff the final result
/// against the into-note's pre-merge properties (see
/// [`count_new_property_keys`]) rather than try to track the restoration as
/// a correction to the fold's own count — a nested owner-established value
/// (an object) makes that correction ill-defined, since the fold's flat
/// "keys contributed" number cannot express a partial reversal of a nested
/// contribution.
pub(crate) fn preserve_owner_established_properties(
    into: &Option<Value>,
    merged: &mut Option<Value>,
) {
    preserve_property_keys(OWNER_ESTABLISHED_PROPERTIES, into, merged);
}

fn preserve_message_transport_properties(into: &Option<Value>, merged: &mut Option<Value>) {
    preserve_property_keys(MESSAGE_TRANSPORT_OWNED_PROPERTIES, into, merged);
}

fn preserve_property_keys(keys: &[&str], into: &Option<Value>, merged: &mut Option<Value>) {
    if !matches!(merged, Some(Value::Object(_))) {
        let Some(Value::Object(into_map)) = into else {
            return;
        };
        let owned_on_into = keys.iter().any(|key| into_map.contains_key(*key));
        if owned_on_into {
            *merged = into.clone();
        }
        return;
    }
    let Some(Value::Object(merged_map)) = merged.as_mut() else {
        return;
    };
    let into_map = match into {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    for key in keys {
        match into_map.and_then(|m| m.get(*key)) {
            Some(value) => {
                // Already present on `into` — restore it verbatim.
                merged_map.insert((*key).to_string(), value.clone());
            }
            None => {
                // Absent from `into` — a value here came from `from` and
                // must not survive the merge.
                merged_map.remove(*key);
            }
        }
    }
}

/// Count properties present in `final_value` that are new relative to
/// `original` — the same "did this key actually get added" question
/// [`merge_json`]'s fold answers, but computed from what the record finally
/// holds rather than carried forward through the fold-then-restore pipeline.
///
/// A key present in both `original` and `final_value` is never counted, even
/// when its value changed — this matches `merge_json`'s own rule that an
/// overwrite of a key already present on `into` is not a merged addition.
/// Nested objects recurse only when the key exists on both sides (mirroring
/// `merge_json`'s `Union` recursion); a key that is wholly new at some level
/// counts once for that level, not once per leaf beneath it.
pub(crate) fn count_new_property_keys(
    original: Option<&Value>,
    final_value: Option<&Value>,
    strategy: EntityDedupMergePolicy,
) -> usize {
    match (original, final_value) {
        (_, None) => 0,
        (None, Some(Value::Object(map))) => map.len(),
        (None, Some(_)) => 1,
        (Some(Value::Object(orig_map)), Some(Value::Object(final_map))) => {
            count_new_keys_within_object(orig_map, final_map, strategy)
        }
        // The record ended up holding an object where it previously held
        // something else. `merge_json` scores that replacement as ONE
        // contribution however many keys the new object carries, and this arm
        // keeps that rule rather than counting the keys — the alternative
        // silently changes `properties_merged` for ordinary notes, which never
        // enter the restoration path and were being reported correctly by the
        // fold. The rule here is: an empty final object has no contribution
        // left to report, whatever emptied it — restoration removing every
        // owner-established key is one way that happens, but an ordinary
        // `PreferFrom` replacement with an empty object reaches this same arm.
        (Some(_), Some(Value::Object(final_map))) => usize::from(!final_map.is_empty()),
        // Whole-value replacement by a non-object. `merge_json` scores a
        // `PreferFrom` fold that replaces one properties value with a
        // differently-shaped one as a single contribution, and that is the right
        // answer: what the record now holds came from the from-note. A bare 0
        // here would under-report every such replacement, including on note
        // kinds that have no owner-established properties and never enter the
        // restoration path at all. Equal values mean nothing was contributed,
        // which is the `properties: Some(a)` merged with `properties: None`
        // case.
        (Some(orig), Some(final_val)) => usize::from(orig != final_val),
    }
}

/// Per-key counting inside a properties object.
///
/// Deliberately NOT the same rule as the top level: within an object, a key that
/// already exists and is merely overwritten counts 0, matching `merge_json`'s
/// rule that only keys absent from the into-note are counted as added.
///
/// Recursion is STRATEGY-AWARE, and it has to be, because `merge_json` only
/// descends into a same-named nested object under [`Union`]. Under `PreferFrom`
/// an existing top-level key is replaced wholesale, and under `PreferInto` it is
/// kept wholesale; in neither case is anything merged *beneath* that key, so
/// descending here would count a nested value that the fold never treated as a
/// separate contribution. Counting `{"meta":{"old":1}}` merged with
/// `{"meta":{"new":2}}` under `PreferFrom` as 1 is exactly that mistake — one
/// existing property was replaced, none was added.
///
/// [`Union`]: EntityDedupMergePolicy::Union
fn count_new_keys_within_object(
    orig_map: &serde_json::Map<String, Value>,
    final_map: &serde_json::Map<String, Value>,
    strategy: EntityDedupMergePolicy,
) -> usize {
    final_map
        .iter()
        .map(|(key, value)| match orig_map.get(key) {
            None => 1,
            Some(Value::Object(nested_orig))
                if matches!(strategy, EntityDedupMergePolicy::Union) =>
            {
                match value {
                    Value::Object(nested_final) => {
                        count_new_keys_within_object(nested_orig, nested_final, strategy)
                    }
                    _ => 0,
                }
            }
            Some(_) => 0,
        })
        .sum()
}

/// Merge two property objects. Returns (merged, count_of_fields_from_from_that_were_added).
/// `pub(crate)` so `crate::atomic_prepare` can reuse this exact properties-merge
/// semantics when building an `update` write plan's row statement, matching
/// `update_entity`/`update_note`'s own patch behavior byte-for-byte.
pub(crate) fn merge_properties(
    into: &Option<Value>,
    from: &Option<Value>,
    strategy: EntityDedupMergePolicy,
) -> (Option<Value>, usize) {
    match (into, from) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(a.clone()), 0),
        (None, Some(b)) => {
            let count = if let Value::Object(m) = b { m.len() } else { 1 };
            (Some(b.clone()), count)
        }
        (Some(into_val), Some(from_val)) => {
            let (merged, added) = merge_json(into_val, from_val, strategy);
            (Some(merged), added)
        }
    }
}

/// Deep-merge two JSON values per strategy. Returns (merged, keys_contributed_by_from).
fn merge_json(into: &Value, from: &Value, strategy: EntityDedupMergePolicy) -> (Value, usize) {
    match (into, from, strategy) {
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::Union) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v_from) in b {
                if let Some(v_into) = a.get(k) {
                    let (merged, sub_added) =
                        merge_json(v_into, v_from, EntityDedupMergePolicy::Union);
                    result.insert(k.clone(), merged);
                    added += sub_added;
                } else {
                    result.insert(k.clone(), v_from.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferInto) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                if !a.contains_key(k) {
                    result.insert(k.clone(), v.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferFrom) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                result.insert(k.clone(), v.clone());
                if !a.contains_key(k) {
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        // Non-object scalars: apply strategy directly.
        (_into_val, from_val, EntityDedupMergePolicy::PreferFrom) => (from_val.clone(), 1),
        _ => (into.clone(), 0),
    }
}

/// `pub(crate)` so `crate::atomic_prepare::prepare_merge` can reuse this for
/// atomic/non-atomic parity.
pub(crate) fn union_tags(into: &[String], from: &[String]) -> (Vec<String>, usize) {
    let mut seen: HashSet<&str> = into.iter().map(|s| s.as_str()).collect();
    let mut result: Vec<String> = into.to_vec();
    let mut added = 0usize;
    for tag in from {
        if seen.insert(tag.as_str()) {
            result.push(tag.clone());
            added += 1;
        }
    }
    (result, added)
}

// ---------------------------------------------------------------------------
// INLINE TEST JUSTIFICATION: tests here exercise patch/merge helpers and the
// update_note/update_entity paths that share private merge_properties logic.
// Moving them to tests/ would require pub-exporting merge_properties, which is
// an internal invariant not suitable for the public API surface. Broad
// behavioral curation tests live in tests/integration.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::runtime::{KhiveRuntime, NamespaceToken};
    use khive_storage::types::{Direction, TextFilter, TextQueryMode, TextSearchRequest};
    use khive_types::EndpointKind;

    fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().unwrap()
    }

    fn outbound_message_note() -> Note {
        let mut note = Note::new("local", "message", "hello");
        note.properties = Some(serde_json::json!({"direction": "outbound"}));
        note
    }

    /// Predicate + ordering contract of the non-wire outbox scan: outbound
    /// with absent OR explicitly-null `delivered_at` is undelivered; a
    /// non-null `delivered_at`, a terminal `delivery` state, an inbound row,
    /// and a soft-deleted row are all excluded; results come newest-first
    /// and respect `limit`; `limit=0` returns nothing.
    #[tokio::test]
    async fn list_undelivered_outbound_messages_predicate_and_order() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let store = rt.notes(&tok).expect("note store");

        let mut undelivered_old = outbound_message_note();
        undelivered_old.created_at -= 10;
        let mut undelivered_null = outbound_message_note();
        undelivered_null.properties =
            Some(serde_json::json!({"direction": "outbound", "delivered_at": null}));
        let mut delivered = outbound_message_note();
        delivered.properties = Some(
            serde_json::json!({"direction": "outbound", "delivered_at": "2026-08-28T00:00:00Z"}),
        );
        // ADR-122 terminal states without `delivered_at` are not pending.
        let mut terminal_failed = outbound_message_note();
        terminal_failed.properties = Some(
            serde_json::json!({"direction": "outbound", "delivery": "failed", "last_error": "x"}),
        );
        let mut inbound = Note::new("local", "message", "inbound row");
        inbound.properties = Some(serde_json::json!({"direction": "inbound"}));
        let mut soft_deleted = outbound_message_note();
        soft_deleted.deleted_at = Some(chrono::Utc::now().timestamp_micros());

        let old_id = undelivered_old.id;
        let null_id = undelivered_null.id;
        for note in [
            undelivered_old,
            undelivered_null,
            delivered,
            terminal_failed,
            inbound,
            soft_deleted,
        ] {
            store.upsert_note(note).await.expect("seed note");
        }

        let hits = rt
            .list_undelivered_outbound_messages(&tok, None, 200)
            .await
            .expect("scan succeeds");
        let ids: Vec<_> = hits.iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            vec![null_id, old_id],
            "only the two undelivered outbound rows, newest-first"
        );

        let capped = rt
            .list_undelivered_outbound_messages(&tok, None, 1)
            .await
            .expect("capped scan succeeds");
        assert_eq!(
            capped.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![null_id],
            "limit truncates after the newest undelivered row"
        );

        let zero = rt
            .list_undelivered_outbound_messages(&tok, None, 0)
            .await
            .expect("zero-limit scan succeeds");
        assert!(zero.is_empty(), "limit=0 returns no rows, not one");
    }

    /// The channel prefix runs BEFORE the limit: a backlog of another
    /// channel's pending rows must not consume the scan budget and starve
    /// the requested channel (the pre-fix defect: filter-after-limit).
    #[tokio::test]
    async fn list_undelivered_outbound_messages_prefix_filters_before_limit() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let store = rt.notes(&tok).expect("note store");

        // Older email row behind three newer telegram rows.
        let mut email = outbound_message_note();
        email.created_at -= 100;
        email.properties =
            Some(serde_json::json!({"direction": "outbound", "to_actor": "email:a@b.c"}));
        let email_id = email.id;
        store.upsert_note(email).await.expect("seed email");
        for _ in 0..3 {
            let mut tg = outbound_message_note();
            tg.properties =
                Some(serde_json::json!({"direction": "outbound", "to_actor": "telegram:42"}));
            store.upsert_note(tg).await.expect("seed telegram");
        }

        // With filter-after-limit this would return a telegram row (newest
        // first) and the email loop would see nothing deliverable.
        let hits = rt
            .list_undelivered_outbound_messages(&tok, Some("email:"), 1)
            .await
            .expect("scan succeeds");
        assert_eq!(
            hits.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![email_id],
            "prefix predicate applies before the limit"
        );

        let telegram_hits = rt
            .list_undelivered_outbound_messages(&tok, Some("telegram:"), 200)
            .await
            .expect("scan succeeds");
        assert_eq!(telegram_hits.len(), 3, "telegram prefix sees its own rows");
    }

    /// Terminal-outcome markers: `delivered` stamps the ADR-122 §1 property
    /// set (with `transport_message_id` only when given), `failed` stamps
    /// §2's permanent-failure set, and both refuse targets that are not live
    /// outbound message notes.
    #[tokio::test]
    async fn outbound_delivery_markers_stamp_adr122_properties_and_validate_target() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let store = rt.notes(&tok).expect("note store");

        let delivered_note = outbound_message_note();
        let delivered_id = delivered_note.id;
        let failed_note = outbound_message_note();
        let failed_id = failed_note.id;
        let mut inbound = Note::new("local", "message", "inbound row");
        inbound.properties = Some(serde_json::json!({"direction": "inbound"}));
        let inbound_id = inbound.id;
        let wrong_kind = Note::new("local", "observation", "not a message");
        let wrong_kind_id = wrong_kind.id;
        for note in [delivered_note, failed_note] {
            store.upsert_note(note).await.expect("seed note");
        }
        store.upsert_note(inbound).await.expect("seed inbound");
        store
            .upsert_note(wrong_kind)
            .await
            .expect("seed non-message");

        let marked = rt
            .mark_outbound_message_delivered(
                &tok,
                delivered_id,
                "2026-08-28T00:00:00Z".to_string(),
                Some("<mid@example>".to_string()),
            )
            .await
            .expect("mark delivered succeeds");
        let props = marked
            .properties
            .as_ref()
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            props.get("delivery").and_then(|v| v.as_str()),
            Some("delivered")
        );
        assert_eq!(
            props.get("delivered_at").and_then(|v| v.as_str()),
            Some("2026-08-28T00:00:00Z")
        );
        assert_eq!(
            props.get("transport_message_id").and_then(|v| v.as_str()),
            Some("<mid@example>")
        );

        let failed = rt
            .mark_outbound_message_failed(
                &tok,
                failed_id,
                "2026-08-28T00:00:01Z".to_string(),
                "recipient not in allowlist".to_string(),
            )
            .await
            .expect("mark failed succeeds");
        let props = failed
            .properties
            .as_ref()
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            props.get("delivery").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert_eq!(
            props.get("failed_at").and_then(|v| v.as_str()),
            Some("2026-08-28T00:00:01Z")
        );
        assert_eq!(
            props.get("last_error").and_then(|v| v.as_str()),
            Some("recipient not in allowlist")
        );

        // Both marked rows are now terminal: the scan must not return them.
        let pending = rt
            .list_undelivered_outbound_messages(&tok, None, 200)
            .await
            .expect("scan succeeds");
        assert!(
            pending.is_empty(),
            "terminal rows left in scan: {:?}",
            pending.iter().map(|n| n.id).collect::<Vec<_>>()
        );

        // Validation arms: inbound message and non-message kind both refuse.
        for (id, label) in [(inbound_id, "inbound"), (wrong_kind_id, "non-message")] {
            let err = rt
                .mark_outbound_message_delivered(&tok, id, "t".to_string(), None)
                .await
                .expect_err(label);
            assert!(
                matches!(err, RuntimeError::InvalidInput(_)),
                "{label}: expected InvalidInput, got {err:?}"
            );
            let err = rt
                .mark_outbound_message_failed(&tok, id, "t".to_string(), "e".to_string())
                .await
                .expect_err(label);
            assert!(
                matches!(err, RuntimeError::InvalidInput(_)),
                "{label}: expected InvalidInput, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn claim_outbound_message_external_id_sets_value_and_survives_readback() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let note = outbound_message_note();
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let claimed = rt
            .claim_outbound_message_external_id(&tok, note_id, "<abc@example.com>".to_string())
            .await
            .expect("claim succeeds on a fresh outbound message note");
        assert_eq!(
            claimed
                .properties
                .as_ref()
                .and_then(|p| p.get("external_id"))
                .and_then(|v| v.as_str()),
            Some("<abc@example.com>")
        );

        // Reads the persisted row back independently of the claim call's own
        // return value. This is the check that fails if the fix is reverted
        // to routing the claim through `dispatch("update", ...)`: that path is
        // refused by the owner-established-property gate exercised in
        // `generic_update_still_refuses_external_id_on_message_note` below, so
        // external_id would never actually persist and this read would come
        // back `None`.
        let reread = rt
            .notes(&tok)
            .expect("note store")
            .get_note(note_id)
            .await
            .expect("read note")
            .expect("note still exists");
        assert_eq!(
            reread
                .properties
                .as_ref()
                .and_then(|p| p.get("external_id"))
                .and_then(|v| v.as_str()),
            Some("<abc@example.com>")
        );
    }

    #[tokio::test]
    async fn generic_update_still_refuses_external_id_on_message_note() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let note = outbound_message_note();
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let err = rt
            .update_note(
                &tok,
                note_id,
                NotePatch {
                    properties: Some(serde_json::json!({"external_id": "<forged@example.com>"})),
                    ..Default::default()
                },
            )
            .await
            .expect_err("caller-facing update must keep refusing external_id on a message note");
        assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
        assert!(err.to_string().contains("is not patchable"), "error: {err}");

        // The owner path is unaffected by the caller-side refusal above.
        let claimed = rt
            .claim_outbound_message_external_id(&tok, note_id, "<claimed@example.com>".to_string())
            .await
            .expect("owner-bookkeeping path still claims after a refused caller patch");
        assert_eq!(
            claimed
                .properties
                .as_ref()
                .and_then(|p| p.get("external_id"))
                .and_then(|v| v.as_str()),
            Some("<claimed@example.com>")
        );
    }

    #[tokio::test]
    async fn claim_refuses_non_message_note() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let mut note = Note::new("local", "observation", "not a message");
        note.properties = Some(serde_json::json!({"direction": "outbound"}));
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let err = rt
            .claim_outbound_message_external_id(&tok, note_id, "<x@example.com>".to_string())
            .await
            .expect_err("a non-message note must never accept the claim");
        assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
    }

    #[tokio::test]
    async fn claim_refuses_inbound_message() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let mut note = Note::new("local", "message", "inbound content");
        note.properties = Some(serde_json::json!({"direction": "inbound"}));
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let err = rt
            .claim_outbound_message_external_id(&tok, note_id, "<x@example.com>".to_string())
            .await
            .expect_err("an inbound message note must never accept the claim");
        assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
    }

    #[tokio::test]
    async fn claim_refuses_when_external_id_already_set() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let mut note = outbound_message_note();
        note.properties = Some(
            serde_json::json!({"direction": "outbound", "external_id": "<already@example.com>"}),
        );
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let err = rt
            .claim_outbound_message_external_id(&tok, note_id, "<new@example.com>".to_string())
            .await
            .expect_err("a note that already carries external_id must refuse re-claim");
        assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
    }

    #[tokio::test]
    async fn generic_update_can_still_patch_delivered_at_on_message_note() {
        let rt = rt();
        rt.install_pack_owned_note_kinds(vec!["message".to_string()]);
        let tok = NamespaceToken::local();
        let note = outbound_message_note();
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let updated = rt
            .update_note(
                &tok,
                note_id,
                NotePatch {
                    properties: Some(serde_json::json!({"delivered_at": "2026-08-09T00:00:00Z"})),
                    ..Default::default()
                },
            )
            .await
            .expect("delivered_at is not owner-established and must remain patchable");
        assert_eq!(
            updated
                .properties
                .as_ref()
                .and_then(|p| p.get("delivered_at"))
                .and_then(|v| v.as_str()),
            Some("2026-08-09T00:00:00Z")
        );
    }

    fn secret_shaped_reason() -> String {
        const ALPHANUMERIC: &[u8] =
            b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        let candidate: String = (0..48)
            .map(|index| char::from(ALPHANUMERIC[(index * 17 + 11) % ALPHANUMERIC.len()]))
            .collect();
        format!("secret value: {candidate}")
    }

    #[test]
    fn note_embedding_text_ref_borrows_stored_content() {
        let note = Note::new("embedding-borrow", "observation", "borrow this content");
        let text = note_embedding_text_ref(&note);

        assert_eq!(text, note.content.as_str());
        assert!(
            std::ptr::eq(text, note.content.as_str()),
            "internal canonical note text must borrow instead of cloning"
        );
        let owned: String = note_embedding_text(&note);
        assert_eq!(owned.as_str(), note.content.as_str());
    }

    #[tokio::test]
    async fn generic_note_update_errors_when_revision_is_already_i64_max() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let mut note = Note::new("local", "observation", "saturated revision");
        note.updated_at = i64::MAX;
        let note_id = note.id;
        rt.notes(&tok)
            .expect("note store")
            .upsert_note(note.clone())
            .await
            .expect("seed saturated note");

        let error = rt
            .update_note(
                &tok,
                note_id,
                NotePatch::new(None, Some("must not land".to_string()), None, None, None),
            )
            .await
            .expect_err("i64::MAX cannot yield a strictly newer CAS revision");
        assert!(
            matches!(&error, RuntimeError::Internal(_)),
            "revision exhaustion is an internal persisted-state error: {error}"
        );
        assert!(error.to_string().contains("i64::MAX"), "error: {error}");

        let persisted = rt
            .notes(&tok)
            .expect("note store")
            .get_note(note_id)
            .await
            .expect("read note")
            .expect("note remains live");
        assert_eq!(persisted, note, "failed revision advance must not mutate");
    }

    async fn restore_edge_preimage(
        rt: &KhiveRuntime,
        token: &NamespaceToken,
        preimage: &MergeEdgePreimage,
    ) {
        let edge = khive_storage::types::Edge {
            id: preimage.id.into(),
            namespace: preimage.namespace.clone(),
            source_id: preimage.source_id,
            target_id: preimage.target_id,
            relation: preimage.relation.parse().expect("stored relation"),
            weight: preimage.weight,
            created_at: chrono::DateTime::from_timestamp_micros(preimage.created_at)
                .expect("stored created_at"),
            updated_at: chrono::DateTime::from_timestamp_micros(preimage.updated_at)
                .expect("stored updated_at"),
            deleted_at: preimage.deleted_at.map(|value| {
                chrono::DateTime::from_timestamp_micros(value).expect("stored deleted_at")
            }),
            metadata: preimage.metadata.clone(),
            target_backend: preimage.target_backend.clone(),
        };
        rt.graph(token)
            .expect("graph store")
            .upsert_edge(edge)
            .await
            .expect("restore edge preimage");
    }

    fn assert_edge_matches_preimage(
        edge: &khive_storage::types::Edge,
        preimage: &MergeEdgePreimage,
    ) {
        assert_eq!(Uuid::from(edge.id), preimage.id);
        assert_eq!(edge.namespace, preimage.namespace);
        assert_eq!(edge.source_id, preimage.source_id);
        assert_eq!(edge.target_id, preimage.target_id);
        assert_eq!(edge.relation.to_string(), preimage.relation);
        assert_eq!(edge.weight, preimage.weight);
        assert_eq!(edge.created_at.timestamp_micros(), preimage.created_at);
        assert_eq!(edge.updated_at.timestamp_micros(), preimage.updated_at);
        assert_eq!(
            edge.deleted_at.map(|value| value.timestamp_micros()),
            preimage.deleted_at
        );
        assert_eq!(edge.metadata, preimage.metadata);
        assert_eq!(edge.target_backend, preimage.target_backend);
    }

    // Helper: search FTS5 for `query` in a runtime namespace.
    async fn fts_hit(rt: &KhiveRuntime, token: &NamespaceToken, query: &str) -> Vec<Uuid> {
        let ns = token.namespace().as_str().to_string();
        rt.text(token)
            .unwrap()
            .search(TextSearchRequest {
                query: query.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 50,
                snippet_chars: 100,
            })
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.subject_id)
            .collect()
    }

    #[tokio::test]
    async fn update_entity_patch_changes_only_specified_fields() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "OriginalName",
                Some("orig desc"),
                Some(serde_json::json!({"k":"v"})),
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    description: Some(Some("new desc".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "OriginalName");
        assert_eq!(updated.description.as_deref(), Some("new desc"));
        assert_eq!(updated.properties, Some(serde_json::json!({"k":"v"})));
    }

    #[tokio::test]
    async fn update_entity_type_patch_validates_preserves_fields_and_requires_reindex() {
        let rt = rt();
        rt.install_entity_type_validator(std::sync::Arc::new(|kind, entity_type| {
            let Some(raw) = entity_type else {
                return Ok(None);
            };
            let normalized = raw.trim().to_ascii_lowercase();
            if kind == "concept" && normalized == "algorithm" {
                Ok(Some(normalized))
            } else {
                Err(RuntimeError::InvalidInput(format!(
                    "unknown entity_type {raw:?} for {kind:?}; valid: algorithm"
                )))
            }
        }));
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "HistoricalAlgorithm",
                Some("keep description"),
                Some(serde_json::json!({"type": "algorithm", "keep": true})),
                vec!["keep-tag".to_string()],
            )
            .await
            .unwrap();

        let (
            prepared,
            reindex_required,
            changed_fields,
            _expected_updated_at,
            _expected_deleted_at,
        ) = rt
            .prepare_update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    entity_type: Some(Some(" Algorithm ".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect("registered entity_type must validate");

        assert_eq!(prepared.entity_type.as_deref(), Some("algorithm"));
        assert_eq!(prepared.name, "HistoricalAlgorithm");
        assert_eq!(prepared.description.as_deref(), Some("keep description"));
        assert_eq!(
            prepared.properties,
            Some(serde_json::json!({"type": "algorithm", "keep": true}))
        );
        assert_eq!(prepared.tags, vec!["keep-tag"]);
        assert!(
            reindex_required,
            "changing entity_type must request the normal entity reindex path"
        );
        assert_eq!(changed_fields, vec!["entity_type"]);

        let err = rt
            .prepare_update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    entity_type: Some(Some("not_registered".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect_err("unregistered entity_type must be rejected");
        assert!(matches!(err, RuntimeError::InvalidInput(_)), "error: {err}");
    }

    /// Regression for the entity lost-update race (khive #1753): two writers
    /// read the same entity revision, then commit successive patches to
    /// independent properties fields. Before the guarded
    /// `replace_entity_if_unchanged` primitive, `update_entity_with_embedding_report`
    /// wrote an unconditional `entity_upsert_statement`, so both patches
    /// "succeeded" and the second silently discarded the first's field
    /// (`a=1` was overwritten back to `a=0` when B's stale full-row replace
    /// landed). This test forces the interleaving with a `Barrier` — both
    /// readers are released together, so both `prepare_update_entity` calls
    /// observe the SAME pre-write revision (asserted below) — then commits
    /// deterministically in a fixed A-then-B order. It reddens if the guard
    /// is dropped from `entity_replace_if_unchanged_statement` ENTIRELY: with
    /// an unconditional UPDATE (or `entity_upsert_statement`), B's write would
    /// also return `true` and `b` would be lost from the final properties.
    ///
    /// It does NOT redden when only `?8 > updated_at` is removed: with
    /// `updated_at = ?13` intact, B is refused by the revision guard whatever
    /// the clock did, so this fixture cannot see that conjunct disappear.
    ///
    /// It cannot ATTRIBUTE a failure to `updated_at = ?13` either, but for a
    /// different reason, and the difference matters. Both racers take their
    /// replacement revision from `prepare_update_entity`'s
    /// `max(now_micros, expected + 1)` above; this test pins the two EXPECTED
    /// revisions equal, never the two REPLACEMENT revisions. So with
    /// `updated_at = ?13` removed, whether B is still refused depends on
    /// whether B's wall-clock read happened to exceed A's committed revision.
    /// That is a race, not a property of the fixture, and no single run of it
    /// establishes either answer.
    ///
    /// Attribution therefore comes from fixtures that force the question:
    /// `entity_cas_refuses_a_replacement_revision_that_does_not_advance` for
    /// the strict-advance conjunct, and
    /// `production_update_entity_refuses_concurrent_stale_writer` for the
    /// production wiring. Making this fixture attribute as well would mean
    /// pinning both replacement revisions to a common `expected + 1`; it is
    /// deliberately left as a whole-guard test instead.
    ///
    /// SCOPE: this exercises the STORE PRIMITIVE directly and never invokes
    /// `update_entity`, so it stays green if the production caller is reverted
    /// to an unconditional write. The wiring is covered separately by
    /// `production_update_entity_refuses_concurrent_stale_writer`; both are
    /// required, neither substitutes for the other.
    #[tokio::test]
    async fn concurrent_entity_property_patches_from_one_revision_only_one_survives() {
        let rt = Arc::new(rt());
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "RaceTarget",
                None,
                Some(serde_json::json!({"a": 0, "b": 0})),
                vec![],
            )
            .await
            .expect("seed entity");
        let id = entity.id;

        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let reader_a = {
            let rt = Arc::clone(&rt);
            let tok = tok.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                rt.prepare_update_entity(
                    &tok,
                    id,
                    EntityPatch {
                        properties: Some(serde_json::json!({"a": 1})),
                        ..Default::default()
                    },
                )
                .await
            })
        };
        let reader_b = {
            let rt = Arc::clone(&rt);
            let tok = tok.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                rt.prepare_update_entity(
                    &tok,
                    id,
                    EntityPatch {
                        properties: Some(serde_json::json!({"b": 1})),
                        ..Default::default()
                    },
                )
                .await
            })
        };

        let (entity_a, _, _, expected_updated_at_a, expected_deleted_at_a) =
            reader_a.await.unwrap().expect("reader A prepares");
        let (entity_b, _, _, expected_updated_at_b, expected_deleted_at_b) =
            reader_b.await.unwrap().expect("reader B prepares");
        assert_eq!(
            expected_updated_at_a, expected_updated_at_b,
            "both readers must observe the same pre-write revision for this to be a real race"
        );

        let store = rt.entities(&tok).expect("entity store");
        assert!(
            store
                .replace_entity_if_unchanged(entity_a, expected_updated_at_a, expected_deleted_at_a)
                .await
                .expect("writer A CAS query"),
            "the first committer from a shared revision must win"
        );
        assert!(
            !store
                .replace_entity_if_unchanged(entity_b, expected_updated_at_b, expected_deleted_at_b)
                .await
                .expect("writer B CAS query"),
            "the second committer from the SAME stale revision must be refused, not merged"
        );

        let final_entity = rt.get_entity(&tok, id).await.expect("read final entity");
        assert_eq!(
            final_entity.properties,
            Some(serde_json::json!({"a": 1, "b": 0})),
            "writer B's field must not be silently merged into the persisted row: {:?}",
            final_entity.properties
        );
    }

    /// Same race as `concurrent_entity_property_patches_from_one_revision_only_one_survives`,
    /// but driven entirely through the PRODUCTION entry point
    /// (`update_entity_with_embedding_report`) rather than the store's
    /// `replace_entity_if_unchanged` primitive directly. This closes a gap
    /// the primitive-level test cannot: it would still pass unchanged if the
    /// production caller were reverted to an unconditional write, since it
    /// never invokes that caller at all. Uses `race_seam::pause_after_read`
    /// (test-only, compiled out of non-test builds) to force both concurrent
    /// callers to observe the identical pre-write revision deterministically —
    /// no sleeps, no reliance on scheduler ordering.
    #[tokio::test]
    async fn production_update_entity_refuses_concurrent_stale_writer() {
        let rt = Arc::new(rt());
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "ProductionRaceTarget",
                None,
                Some(serde_json::json!({"a": 0, "b": 0})),
                vec![],
            )
            .await
            .expect("seed entity");
        let id = entity.id;

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

        let writer_a = {
            let rt = Arc::clone(&rt);
            let tok = tok.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            tokio::spawn(race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
                rt.update_entity_with_embedding_report(
                    &tok,
                    id,
                    EntityPatch {
                        properties: Some(serde_json::json!({"a": 1})),
                        ..Default::default()
                    },
                )
                .await
            }))
        };
        let writer_b = {
            let rt = Arc::clone(&rt);
            let tok = tok.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            tokio::spawn(race_seam::AFTER_READ_BARRIER.scope(barrier, async move {
                rt.update_entity_with_embedding_report(
                    &tok,
                    id,
                    EntityPatch {
                        properties: Some(serde_json::json!({"b": 1})),
                        ..Default::default()
                    },
                )
                .await
            }))
        };

        let result_a = writer_a.await.expect("writer A task");
        let result_b = writer_b.await.expect("writer B task");
        let successes = [result_a.is_ok(), result_b.is_ok()]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            successes, 1,
            "exactly one production caller must win the race; the other must be refused: \
             a={result_a:?} b={result_b:?}"
        );
        let refused = if result_a.is_err() {
            result_a
        } else {
            result_b
        };
        match &refused {
            Err(RuntimeError::Khive(khive_error)) => {
                assert_eq!(
                    khive_error.kind(),
                    khive_types::ErrorKind::Conflict,
                    "the losing production caller must surface a typed conflict, not \
                     silently overwrite: {refused:?}"
                );
            }
            other => panic!("expected a typed conflict error, got {other:?}"),
        }

        let final_entity = rt.get_entity(&tok, id).await.expect("read final entity");
        assert_ne!(
            final_entity.properties,
            Some(serde_json::json!({"a": 1, "b": 1})),
            "both racers' fields must never both land: that would mean the loser's stale \
             write silently succeeded"
        );
    }

    /// Isolating fixture for the `AND ?8 > updated_at` conjunct of
    /// `entity_replace_if_unchanged_statement`.
    ///
    /// The concurrent-race tests above cannot cover it. What is measured, and
    /// deterministic: tautologizing `?8 > updated_at` alone reddened NOTHING in
    /// `khive-runtime` before this test existed, because the revision guard
    /// (`updated_at = ?13`) refuses the losing writer on its own whatever the
    /// clock did. Tautologizing `updated_at = ?13` + `deleted_at IS ?14`
    /// reddens only `production_update_entity_refuses_concurrent_stale_writer`,
    /// and defeating all three at once reddens the race tests.
    ///
    /// What is NOT claimed, because the fixture cannot support it: that the two
    /// guards are each independently sufficient. The race fixture pins the two
    /// racers' EXPECTED revisions equal but never their REPLACEMENT revisions,
    /// which both come from `max(now, expected + 1)`. So with `updated_at = ?13`
    /// removed, whether strict advance still refuses depends on which clock read
    /// won — a race, not a property of the fixture. This test strips the second
    /// mechanism by construction instead:
    /// it supplies the CORRECT expected revision and deletion marker, so
    /// `?13`/`?14` are satisfied by construction, and the ONLY thing that can
    /// refuse the write is the strict-advance conjunct.
    #[tokio::test]
    async fn entity_cas_refuses_a_replacement_revision_that_does_not_advance() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "NonAdvancing",
                None,
                Some(serde_json::json!({"a": 0})),
                vec![],
            )
            .await
            .expect("seed entity");
        let id = entity.id;

        let (mut replacement, _, _, expected_updated_at, expected_deleted_at) = rt
            .prepare_update_entity(
                &tok,
                id,
                EntityPatch {
                    properties: Some(serde_json::json!({"a": 1})),
                    ..Default::default()
                },
            )
            .await
            .expect("prepare update");

        // Force the replacement revision to EQUAL the stored one, then PROVE the
        // isolation rather than asserting it in prose. Reading the row back is
        // what makes `?13` and `?14` observed facts here instead of values
        // carried out of `prepare_update_entity`'s setup read.
        replacement.updated_at = expected_updated_at;

        let store = rt.entities(&tok).expect("entity store");
        let stored = store
            .get_entity_including_deleted(id)
            .await
            .expect("read stored row")
            .expect("row present before CAS");
        assert_eq!(
            stored.updated_at, expected_updated_at,
            "fixture premise: nothing moved the stored revision between prepare and CAS, \
             otherwise `?13` would refuse and this stops being an isolating fixture"
        );
        assert_eq!(
            stored.deleted_at, expected_deleted_at,
            "fixture premise: the stored deletion marker must still equal the snapshot's, \
             otherwise `deleted_at IS ?14` would refuse and this stops being an isolating \
             fixture"
        );
        assert_eq!(
            replacement.updated_at, stored.updated_at,
            "fixture premise: the replacement revision must NOT advance past the stored one, \
             which is the single condition under test"
        );

        let committed = store
            .replace_entity_if_unchanged(replacement, expected_updated_at, expected_deleted_at)
            .await
            .expect("CAS query");
        assert!(
            !committed,
            "a replacement whose revision does not strictly advance past the stored one must \
             be refused: without `?8 > updated_at` the CAS would accept a write that leaves \
             `updated_at` unmoved, so a later writer holding the same snapshot would still \
             see its expected revision match and overwrite this one"
        );

        let stored = rt.get_entity(&tok, id).await.expect("read back");
        assert_eq!(
            stored.properties,
            Some(serde_json::json!({"a": 0})),
            "the refused write must not have landed"
        );
    }

    /// Isolating fixture for the `AND deleted_at IS ?14` conjunct of
    /// `entity_replace_if_unchanged_statement`.
    ///
    /// The revision-based race fixtures cannot reach it, because a soft delete
    /// does not move the revision: `entity_soft_delete_statement` is
    /// `SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL` and never
    /// touches `updated_at`. So a writer holding a pre-delete snapshot still has
    /// the CORRECT expected revision after the row is tombstoned, and its
    /// replacement revision still advances. Every conjunct except `?14` is
    /// satisfied, and dropping `?14` would let that writer's `deleted_at = NULL`
    /// land — resurrecting a tombstone, with no revision conflict anywhere to
    /// signal it.
    ///
    /// This fixture does not merely claim that isolation, it asserts it: both
    /// other conjuncts are checked against the post-delete row before the CAS
    /// runs, so a future change that makes one of them refuse instead turns this
    /// test red rather than silently converting it into a whole-guard test.
    #[tokio::test]
    async fn entity_cas_refuses_a_stale_replacement_that_would_resurrect_a_tombstone() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Tombstoned",
                None,
                Some(serde_json::json!({"a": 0})),
                vec![],
            )
            .await
            .expect("seed entity");
        let id = entity.id;

        // Snapshot BEFORE the delete: this is the stale writer's view.
        let (replacement, _, _, expected_updated_at, expected_deleted_at) = rt
            .prepare_update_entity(
                &tok,
                id,
                EntityPatch {
                    properties: Some(serde_json::json!({"a": 1})),
                    ..Default::default()
                },
            )
            .await
            .expect("prepare update");
        assert_eq!(
            expected_deleted_at, None,
            "fixture premise: the snapshot must be of a LIVE row"
        );

        rt.delete_entity(&tok, id, false)
            .await
            .expect("soft delete");

        let store = rt.entities(&tok).expect("entity store");
        let tombstoned = store
            .get_entity_including_deleted(id)
            .await
            .expect("read tombstone")
            .expect("row still present after soft delete");

        // Prove the isolation rather than asserting it in prose. `?13` matches
        // because the soft delete left the revision alone, and `?8 > updated_at`
        // holds because the prepared replacement advanced past it. That leaves
        // `deleted_at IS ?14` as the only conjunct able to refuse the write.
        assert_eq!(
            tombstoned.updated_at, expected_updated_at,
            "fixture premise: soft delete must NOT move `updated_at`, otherwise \
             `?13` would refuse and this stops being an isolating fixture"
        );
        assert!(
            replacement.updated_at > tombstoned.updated_at,
            "fixture premise: the replacement revision must still advance, \
             otherwise `?8 > updated_at` would refuse and this stops being an \
             isolating fixture"
        );
        assert!(
            tombstoned.deleted_at.is_some(),
            "fixture premise: the row must actually be tombstoned"
        );

        let committed = store
            .replace_entity_if_unchanged(replacement, expected_updated_at, expected_deleted_at)
            .await
            .expect("CAS query");
        assert!(
            !committed,
            "a replacement carrying a pre-delete snapshot must be refused after the row is \
             soft-deleted: without `deleted_at IS ?14` it would write `deleted_at = NULL` over \
             the tombstone and silently resurrect a deleted entity"
        );

        let after = store
            .get_entity_including_deleted(id)
            .await
            .expect("read back")
            .expect("row present");
        assert!(
            after.deleted_at.is_some(),
            "the tombstone must survive the refused write"
        );
        assert_eq!(
            after.properties,
            Some(serde_json::json!({"a": 0})),
            "the refused write must not have landed"
        );
    }

    /// Regression: the entity CAS requires the replacement revision to be
    /// STRICTLY greater than the stored one. If the replacement is computed
    /// as a raw `Utc::now()` read, a stored revision that is at or ahead of
    /// wall-clock time (a clock step backward, or — deterministically,
    /// reproduced here — a stored revision manufactured slightly ahead of
    /// "now") makes the new value fail to advance, and the CAS refuses a
    /// write with NO concurrent writer involved at all. The fix must clamp
    /// the replacement to `max(now, stored + 1)`; this test sets the stored
    /// revision one full second into the future (far outside normal clock
    /// skew) and asserts the update still succeeds instead of surfacing a
    /// spurious conflict.
    ///
    /// SCOPE: this is a revision-clamp test, NOT CAS regression coverage. Its
    /// assertion is that the write SUCCEEDS, which an unconditional UPDATE
    /// also satisfies, so it stays green if the `updated_at = ?13` /
    /// `?8 > updated_at` guard is dropped entirely. The guard's regression
    /// coverage is
    /// `concurrent_entity_property_patches_from_one_revision_only_one_survives`
    /// (primitive) and `production_update_entity_refuses_concurrent_stale_writer`
    /// (production wiring); do not count this test toward it.
    #[tokio::test]
    async fn update_entity_succeeds_when_stored_revision_is_ahead_of_wall_clock() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "FutureRevisionTarget",
                None,
                None,
                vec![],
            )
            .await
            .expect("seed entity");
        let id = entity.id;

        let future_micros = chrono::Utc::now().timestamp_micros() + 1_000_000;
        let pool = rt.backend().pool_arc();
        let id_str = id.to_string();
        tokio::task::spawn_blocking(move || {
            let guard = pool.writer().expect("writer connection");
            guard
                .execute(
                    "UPDATE entities SET updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![future_micros, id_str],
                )
                .expect("force future revision")
        })
        .await
        .expect("join");

        let updated = rt
            .update_entity(
                &tok,
                id,
                EntityPatch {
                    description: Some(Some("patched after a forced future revision".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect(
                "update must succeed and advance past the stored revision, not report a \
                 spurious conflict when nothing else wrote to this row",
            );
        assert!(updated.updated_at > future_micros);
    }

    #[tokio::test]
    async fn update_entity_clear_description_with_some_none() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "ClearDesc",
                Some("has description"),
                None,
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    description: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(
            updated.description.is_none(),
            "description should be cleared"
        );
    }

    #[tokio::test]
    async fn update_entity_reindexes_when_name_changes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "OldName", None, None, vec![])
            .await
            .unwrap();

        let hits_before = fts_hit(&rt, &tok, "OldName").await;
        assert!(
            hits_before.contains(&entity.id),
            "entity should be findable by old name"
        );

        rt.update_entity(
            &tok,
            entity.id,
            EntityPatch {
                name: Some("NewName".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let hits_old = fts_hit(&rt, &tok, "OldName").await;
        let hits_new = fts_hit(&rt, &tok, "NewName").await;

        assert!(
            !hits_old.contains(&entity.id),
            "old name should no longer match after rename"
        );
        assert!(
            hits_new.contains(&entity.id),
            "new name should be findable after rename"
        );
    }

    #[tokio::test]
    async fn update_entity_properties_merges_preserving_existing_keys() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "MergeProps",
                None,
                Some(serde_json::json!({
                    "domain": "inference",
                    "repo": "lattice",
                    "status": "researched",
                })),
                vec![],
            )
            .await
            .unwrap();

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    properties: Some(serde_json::json!({"status": "implemented"})),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let props = updated.properties.expect("properties should remain set");
        assert_eq!(props["domain"], "inference", "domain key must be preserved");
        assert_eq!(props["repo"], "lattice", "repo key must be preserved");
        assert_eq!(
            props["status"], "implemented",
            "status key must be updated by patch"
        );
    }

    #[tokio::test]
    async fn update_entity_skips_reindex_when_only_properties_change() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "StableIndexed", None, None, vec![])
            .await
            .unwrap();

        let hits_before = fts_hit(&rt, &tok, "StableIndexed").await;
        assert!(hits_before.contains(&entity.id));

        rt.update_entity(
            &tok,
            entity.id,
            EntityPatch {
                properties: Some(serde_json::json!({"new": "prop"})),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let hits_after = fts_hit(&rt, &tok, "StableIndexed").await;
        assert!(
            hits_after.contains(&entity.id),
            "still findable after props-only patch"
        );
    }

    #[tokio::test]
    async fn merge_entity_rewires_edges() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&tok, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();
        let d = rt
            .create_entity(&tok, "concept", None, "D", None, None, vec![])
            .await
            .unwrap();

        // A→B and C→B; merge B into D → should become A→D and C→D.
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, c.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                d.id,
                b.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(summary.kept_id, d.id);
        assert_eq!(summary.removed_id, b.id);
        assert_eq!(summary.edges_rewired, 2);

        let a_neighbors = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(a_neighbors.len(), 1);
        assert_eq!(a_neighbors[0].node_id, d.id);

        let c_neighbors = rt
            .neighbors(&tok, c.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(c_neighbors.len(), 1);
        assert_eq!(c_neighbors[0].node_id, d.id);
    }

    // khive#1236: edges incident to `from_id` but stamped with a namespace other
    // than the merge caller's must still be discovered and rewired — by-ID edge
    // endpoints are namespace-agnostic (ADR-007 Rev 6), and `link` stamps an edge
    // with its *creator's* namespace, not either endpoint's.
    #[tokio::test]
    async fn merge_entity_rewires_edges_from_other_namespaces() {
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let into_a = rt
            .create_entity(&ns_a, "concept", None, "Into A", None, None, vec![])
            .await
            .unwrap();
        let from_a = rt
            .create_entity(&ns_a, "concept", None, "From A", None, None, vec![])
            .await
            .unwrap();
        let foreign_b = rt
            .create_entity(&ns_b, "concept", None, "Foreign B", None, None, vec![])
            .await
            .unwrap();

        // Edge created by an ns_b caller, stamped with ns_b, whose target lives in
        // ns_a — legal because by-ID link endpoints are namespace-agnostic.
        rt.link(
            &ns_b,
            foreign_b.id,
            from_a.id,
            EdgeRelation::Extends,
            1.0,
            None,
        )
        .await
        .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &ns_a,
                into_a.id,
                from_a.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 1,
            "the ns_b-stamped edge incident to from_id must be discovered and rewired, not missed"
        );

        let foreign_neighbors = rt
            .neighbors(&ns_b, foreign_b.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(
            foreign_neighbors.len(),
            1,
            "cross-namespace edge must survive the merge, rewired to point at into_id"
        );
        assert_eq!(foreign_neighbors[0].node_id, into_a.id);
    }

    // khive#1216: a merge rewire must re-check the pack endpoint contract for the
    // POST-rewrite pair, not just carry the pre-merge edge over. into_id and
    // from_id share `kind` (enforced by the caller) but may differ in
    // `entity_type`, so a pack rule scoped via `EntityOfType` can accept
    // `from_id`'s edge yet reject the identical relation once rewritten onto
    // `into_id`.
    #[tokio::test]
    async fn merge_entity_drops_edge_violating_endpoint_contract_after_rewire() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // depends_on is NOT in the base concept->concept allowlist; only this
        // pack rule (theorem -> definition) accepts it.
        rt.install_edge_rules(vec![EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source: EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
            target: EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "definition",
            },
        }]);

        let def_entity = rt
            .create_entity(
                &tok,
                "concept",
                Some("definition"),
                "Def",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from_theorem = rt
            .create_entity(
                &tok,
                "concept",
                Some("theorem"),
                "FromTheorem",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        // Same base kind ("concept") as from_theorem, but a different entity_type
        // — the merge's same-kind constraint allows this, the endpoint contract
        // (entity_type-scoped) does not.
        let into_lemma = rt
            .create_entity(
                &tok,
                "concept",
                Some("lemma"),
                "IntoLemma",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        rt.link(
            &tok,
            from_theorem.id,
            def_entity.id,
            EdgeRelation::DependsOn,
            1.0,
            None,
        )
        .await
        .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into_lemma.id,
                from_theorem.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 0,
            "the contract-violating rewire must not be counted as rewired"
        );
        assert_eq!(
            summary.edges_contract_skipped, 1,
            "the depends_on edge must be dropped, not silently rewritten past the endpoint contract"
        );

        let def_neighbors = rt
            .neighbors(&tok, def_entity.id, Direction::In, None, None)
            .await
            .unwrap();
        assert!(
            def_neighbors.is_empty(),
            "no contract-violating depends_on edge should survive onto into_lemma; got {def_neighbors:?}"
        );
    }

    // Dry-run counterpart: a contract-violating rewire must be predicted as
    // skipped (not rewired), and no write occurs.
    #[tokio::test]
    async fn merge_entity_dry_run_predicts_contract_skip_without_writing() {
        let rt = rt();
        let tok = NamespaceToken::local();

        rt.install_edge_rules(vec![EdgeEndpointRule {
            relation: EdgeRelation::DependsOn,
            source: EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
            target: EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "definition",
            },
        }]);

        let def_entity = rt
            .create_entity(
                &tok,
                "concept",
                Some("definition"),
                "Def",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from_theorem = rt
            .create_entity(
                &tok,
                "concept",
                Some("theorem"),
                "FromTheorem",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let into_lemma = rt
            .create_entity(
                &tok,
                "concept",
                Some("lemma"),
                "IntoLemma",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        rt.link(
            &tok,
            from_theorem.id,
            def_entity.id,
            EdgeRelation::DependsOn,
            1.0,
            None,
        )
        .await
        .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into_lemma.id,
                from_theorem.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true, // dry_run
                None,
            )
            .await
            .unwrap();

        assert_eq!(summary.edges_rewired, 0);
        assert_eq!(summary.edges_contract_skipped, 1);

        // Nothing written: the original edge is untouched.
        let def_neighbors = rt
            .neighbors(&tok, def_entity.id, Direction::In, None, None)
            .await
            .unwrap();
        assert_eq!(def_neighbors.len(), 1);
        assert_eq!(def_neighbors[0].node_id, from_theorem.id);
    }

    // A conflicting rewire must leave the surviving edge untouched (ADR-039
    // DO NOTHING) — the merged-from edge's attributes never overwrite it.
    #[tokio::test]
    async fn merge_entity_conflict_keeps_survivor_edge_attributes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        let survivor = rt
            .link(&tok, into.id, shared.id, EdgeRelation::Extends, 0.9, None)
            .await
            .unwrap();
        rt.link(&tok, from.id, shared.id, EdgeRelation::Extends, 0.2, None)
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

        let edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(into.id),
                    target_id: Some(shared.id),
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].id, survivor.id);
        assert!(
            (edges[0].weight - 0.9).abs() < f64::EPSILON,
            "survivor weight must not be overwritten by the merged-from edge; got {}",
            edges[0].weight
        );
    }

    #[tokio::test]
    async fn merge_entity_conflict_records_restorable_edge_and_annotation_preimages() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();
        let annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "edge judgment",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let nested_annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "judgment review",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        let survivor = rt
            .link(
                &tok,
                into.id,
                shared.id,
                EdgeRelation::Extends,
                0.9,
                Some(serde_json::json!({"source": "survivor"})),
            )
            .await
            .unwrap();
        let dropped = rt
            .link(
                &tok,
                from.id,
                shared.id,
                EdgeRelation::Extends,
                0.2,
                Some(serde_json::json!({"source": "dropped"})),
            )
            .await
            .unwrap();
        let annotation = rt
            .link(
                &tok,
                annotator.id,
                dropped.id.into(),
                EdgeRelation::Annotates,
                0.7,
                Some(serde_json::json!({"basis": "manual"})),
            )
            .await
            .unwrap();
        let nested_annotation = rt
            .link(
                &tok,
                nested_annotator.id,
                annotation.id.into(),
                EdgeRelation::Annotates,
                0.6,
                Some(serde_json::json!({"review": "confirmed"})),
            )
            .await
            .unwrap();
        rt.delete_edge(&tok, annotation.id.into(), false)
            .await
            .unwrap();

        let summary = rt
            .merge_entity(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .unwrap();

        let [conflict] = summary.edge_conflict_preimages.as_slice() else {
            panic!(
                "expected one edge-conflict preimage, got {:?}",
                summary.edge_conflict_preimages
            );
        };
        assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
        assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
        assert_eq!(conflict.dropped_edge.source_id, from.id);
        assert_eq!(conflict.dropped_edge.target_id, shared.id);
        assert_eq!(conflict.dropped_edge.relation, "extends");
        assert_eq!(conflict.dropped_edge.weight, 0.2);
        assert_eq!(
            conflict.dropped_edge.metadata,
            Some(serde_json::json!({"source": "dropped"}))
        );
        assert_eq!(conflict.incident_edge_preimages.len(), 2);
        assert_eq!(
            conflict.incident_edge_preimages[0].id,
            Uuid::from(annotation.id)
        );
        assert_eq!(
            conflict.incident_edge_preimages[1].id,
            Uuid::from(nested_annotation.id)
        );

        for id in [dropped.id, annotation.id, nested_annotation.id] {
            assert!(
                rt.get_edge_including_deleted(&tok, id.into())
                    .await
                    .unwrap()
                    .is_none(),
                "merge conflict cascade must leave no dangling edge row for {id}"
            );
        }

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::EntityMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(events.items.len(), 1);
        assert_eq!(
            events.items[0].payload["edge_conflict_preimages"],
            serde_json::to_value(&summary.edge_conflict_preimages).unwrap()
        );

        restore_edge_preimage(&rt, &tok, &conflict.dropped_edge).await;
        for preimage in &conflict.incident_edge_preimages {
            restore_edge_preimage(&rt, &tok, preimage).await;
        }
        for preimage in
            std::iter::once(&conflict.dropped_edge).chain(conflict.incident_edge_preimages.iter())
        {
            let restored = rt
                .get_edge_including_deleted(&tok, preimage.id)
                .await
                .unwrap()
                .expect("restored edge");
            assert_edge_matches_preimage(&restored, preimage);
        }
    }

    // A dry run must predict the same conflict preimages a committing merge
    // would produce, without deleting or mutating a single row. The incident
    // cascade is two levels deep (an annotation on the dropped edge, and a
    // nested annotation on that annotation) so the root-to-leaf ordering
    // ADR-014 promises is actually exercised, not just a one-element vec that
    // trivially satisfies any order. Every row touched by the merge — both
    // entities and every edge — is snapshotted before the dry run and
    // compared field-for-field against its post-run state.
    #[tokio::test]
    async fn merge_entity_dry_run_conflict_returns_preimages_without_mutating() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();
        let annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "edge judgment",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let nested_annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "judgment review",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        let survivor = rt
            .link(
                &tok,
                into.id,
                shared.id,
                EdgeRelation::Extends,
                0.9,
                Some(serde_json::json!({"source": "survivor"})),
            )
            .await
            .unwrap();
        let dropped = rt
            .link(
                &tok,
                from.id,
                shared.id,
                EdgeRelation::Extends,
                0.2,
                Some(serde_json::json!({"source": "dropped"})),
            )
            .await
            .unwrap();
        let annotation = rt
            .link(
                &tok,
                annotator.id,
                dropped.id.into(),
                EdgeRelation::Annotates,
                0.7,
                Some(serde_json::json!({"basis": "manual"})),
            )
            .await
            .unwrap();
        let nested_annotation = rt
            .link(
                &tok,
                nested_annotator.id,
                annotation.id.into(),
                EdgeRelation::Annotates,
                0.6,
                Some(serde_json::json!({"basis": "nested"})),
            )
            .await
            .unwrap();
        rt.delete_edge(&tok, nested_annotation.id.into(), false)
            .await
            .unwrap();

        let survivor_before = rt
            .get_edge_including_deleted(&tok, survivor.id.into())
            .await
            .unwrap()
            .expect("survivor edge exists");
        let dropped_before = rt
            .get_edge_including_deleted(&tok, dropped.id.into())
            .await
            .unwrap()
            .expect("dropped edge exists");
        let annotation_before = rt
            .get_edge_including_deleted(&tok, annotation.id.into())
            .await
            .unwrap()
            .expect("annotation edge exists");
        let nested_annotation_before = rt
            .get_edge_including_deleted(&tok, nested_annotation.id.into())
            .await
            .unwrap()
            .expect("nested annotation edge exists");
        let into_before = rt
            .get_entity(&tok, into.id)
            .await
            .expect("into entity exists");
        let from_before = rt
            .get_entity(&tok, from.id)
            .await
            .expect("from entity exists");

        let summary = rt
            .merge_entity(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
            )
            .await
            .unwrap();

        let [conflict] = summary.edge_conflict_preimages.as_slice() else {
            panic!(
                "expected one edge-conflict preimage from the dry run, got {:?}",
                summary.edge_conflict_preimages
            );
        };
        assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
        assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
        assert_eq!(conflict.dropped_edge.source_id, from.id);
        assert_eq!(conflict.dropped_edge.target_id, shared.id);
        assert_eq!(conflict.dropped_edge.weight, 0.2);
        // Root-to-leaf order (ADR-014): the direct annotation on the dropped
        // edge must precede the annotation nested on top of it.
        assert_eq!(conflict.incident_edge_preimages.len(), 2);
        assert_eq!(
            conflict.incident_edge_preimages[0].id,
            Uuid::from(annotation.id)
        );
        assert!(
            conflict.incident_edge_preimages[0].deleted_at.is_none(),
            "the direct annotation was never soft-deleted"
        );
        assert_eq!(
            conflict.incident_edge_preimages[1].id,
            Uuid::from(nested_annotation.id)
        );
        assert!(
            conflict.incident_edge_preimages[1].deleted_at.is_some(),
            "dry-run preimage must retain the nested annotation's tombstone state"
        );

        let survivor_after = rt
            .get_edge_including_deleted(&tok, survivor.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the survivor edge");
        let dropped_after = rt
            .get_edge_including_deleted(&tok, dropped.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the dropped edge");
        let annotation_after = rt
            .get_edge_including_deleted(&tok, annotation.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the cascaded annotation");
        let nested_annotation_after = rt
            .get_edge_including_deleted(&tok, nested_annotation.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the nested cascaded annotation");
        assert_eq!(
            serde_json::to_value(&survivor_before).unwrap(),
            serde_json::to_value(&survivor_after).unwrap(),
            "dry run must not mutate the surviving edge's row at all"
        );
        assert_eq!(
            serde_json::to_value(&dropped_before).unwrap(),
            serde_json::to_value(&dropped_after).unwrap(),
            "dry run must not mutate the would-be-dropped edge's row at all"
        );
        assert_eq!(
            serde_json::to_value(&annotation_before).unwrap(),
            serde_json::to_value(&annotation_after).unwrap(),
            "dry run must not mutate the incident annotation's row at all"
        );
        assert_eq!(
            serde_json::to_value(&nested_annotation_before).unwrap(),
            serde_json::to_value(&nested_annotation_after).unwrap(),
            "dry run must not mutate the nested incident annotation's row at all"
        );

        let into_after = rt
            .get_entity(&tok, into.id)
            .await
            .expect("into entity must remain unmerged after a dry run");
        let from_after = rt
            .get_entity(&tok, from.id)
            .await
            .expect("from entity must not be merged away by a dry run");
        assert_eq!(
            serde_json::to_value(&into_before).unwrap(),
            serde_json::to_value(&into_after).unwrap(),
            "dry run must not mutate the into entity's row at all"
        );
        assert_eq!(
            serde_json::to_value(&from_before).unwrap(),
            serde_json::to_value(&from_after).unwrap(),
            "dry run must not mutate the from entity's row at all"
        );
        assert_eq!(from_after.deleted_at, None);
        assert_eq!(from_after.merged_into, None);
        assert_eq!(from_after.merge_event_id, None);

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::EntityMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(
            events.items.is_empty(),
            "a dry run must not record a merge audit event"
        );
    }

    // A soft-deleted surviving edge must not be resurrected by a conflicting
    // rewire — the from-edge is dropped and the tombstone stays.
    #[tokio::test]
    async fn merge_entity_conflict_does_not_resurrect_tombstoned_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        let survivor = rt
            .link(&tok, into.id, shared.id, EdgeRelation::Extends, 0.9, None)
            .await
            .unwrap();
        rt.delete_edge(&tok, survivor.id.into(), false)
            .await
            .unwrap();
        rt.link(&tok, from.id, shared.id, EdgeRelation::Extends, 0.2, None)
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

        for (src, label) in [(into.id, "into"), (from.id, "from")] {
            let edges = rt
                .list_edges(
                    &tok,
                    crate::EdgeListFilter {
                        source_id: Some(src),
                        target_id: Some(shared.id),
                        relations: vec![EdgeRelation::Extends],
                        ..Default::default()
                    },
                    10,
                    0,
                )
                .await
                .unwrap();
            assert!(
                edges.is_empty(),
                "no live {label}→shared edge may exist after merging over a tombstone; got: {edges:?}"
            );
        }
    }

    // The survivor row write must not null columns it doesn't merge —
    // entity_type (and the old entity-owned content_ref) were lost by the old full-row
    // INSERT OR REPLACE.
    #[tokio::test]
    async fn merge_entity_preserves_survivor_entity_type() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "resource", Some("skill"), "Into", None, None, vec![])
            .await
            .unwrap();
        assert_eq!(into.entity_type.as_deref(), Some("skill"));
        let from = rt
            .create_entity(&tok, "resource", None, "From", None, None, vec![])
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

        let got = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(
            got.entity_type.as_deref(),
            Some("skill"),
            "merge must not null the survivor's entity_type"
        );
    }

    #[tokio::test]
    async fn merge_entity_preserves_survivor_content_ref() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "document", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "document", None, "From", None, None, vec![])
            .await
            .unwrap();

        let content_ref = khive_storage::ContentRef::from_hex("0".repeat(64)).unwrap();
        let store = rt.entities(&tok).unwrap();
        rt.attachments()
            .unwrap()
            .upsert_attachment(khive_storage::Attachment::from_new(
                into.id,
                khive_storage::AttachmentSubstrate::Entity,
                khive_storage::NewAttachment {
                    role: "content".to_string(),
                    content_ref: content_ref.clone(),
                    media_type: None,
                    size_bytes: None,
                },
                into.created_at,
            ))
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

        let got = store.get_entity(into.id).await.unwrap().unwrap();
        assert_eq!(
            got.content_ref.as_deref(),
            Some(content_ref.as_str()),
            "merge must not null the survivor's content_ref"
        );
    }

    #[tokio::test]
    async fn merge_entity_self_merge_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let err = rt
            .merge_entity_with_reason(
                &tok,
                a.id,
                a.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("cannot merge an entity into itself"),
            "expected self-merge rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn merge_entity_prefer_into_strategy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let props = kept.properties.unwrap();
        // a stays as 1 (into wins), b is added from from.
        assert_eq!(props["a"], 1);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_prefer_from_strategy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferFrom,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let props = kept.properties.unwrap();
        // from wins on a, b also from from.
        assert_eq!(props["a"], 2);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_union_strategy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                Some(serde_json::json!({"a": 1})),
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({"a": 2, "b": 3})),
                vec![],
            )
            .await
            .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::Union,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let props = kept.properties.unwrap();
        // Scalar conflict: into wins → a=1. b added from from.
        assert_eq!(props["a"], 1);
        assert_eq!(props["b"], 3);
    }

    #[tokio::test]
    async fn merge_entity_unions_tags() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Into",
                None,
                None,
                vec!["x".to_string(), "y".to_string()],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                None,
                vec!["y".to_string(), "z".to_string()],
            )
            .await
            .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        let mut tags = kept.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["x", "y", "z"]);
    }

    #[tokio::test]
    async fn merge_entity_drops_self_loops() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        // A `extends` B — merging B into A would produce A `extends` A → drop it.
        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                a.id,
                b.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 0,
            "self-loop should be dropped, not rewired"
        );

        let a_out = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(a_out.is_empty(), "no self-loop should remain");
    }

    // ---- content_strategy for entity merge ----

    #[tokio::test]
    async fn merge_entity_append_strategy_concatenates_descriptions() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(
            summary.content_appended,
            "append strategy with two non-empty descriptions must report content_appended=true"
        );
        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(kept.description.as_deref(), Some("desc A\n\n---\n\ndesc B"));
    }

    #[tokio::test]
    async fn merge_entity_append_strategy_from_empty_is_noop() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(
            !summary.content_appended,
            "from's empty description means nothing was appended"
        );
        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(kept.description.as_deref(), Some("desc A"));
    }

    #[tokio::test]
    async fn merge_entity_append_strategy_into_empty_takes_from() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(
            summary.content_appended,
            "taking from's description into an empty into is real content preservation"
        );
        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(kept.description.as_deref(), Some("desc B"));
    }

    #[tokio::test]
    async fn merge_entity_prefer_into_strategy_still_discards_explicitly() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::PreferInto,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(
            !summary.content_appended,
            "explicit PreferInto opt-out must not report an append"
        );
        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(
            kept.description.as_deref(),
            Some("desc A"),
            "explicit PreferInto opt-out keeps the old discard behavior"
        );
    }

    /// `content_strategy` must be followed directly, independent of the
    /// entity-field `strategy`: with the default entity policy `prefer_into`,
    /// an explicit `content_strategy=prefer_from` must still keep the
    /// from-description.
    #[tokio::test]
    async fn merge_entity_prefer_from_content_strategy_wins_over_default_entity_policy() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::PreferFrom,
                false,
                None,
            )
            .await
            .unwrap();

        assert!(
            !summary.content_appended,
            "explicit PreferFrom is not an append"
        );
        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(
            kept.description.as_deref(),
            Some("desc B"),
            "content_strategy=prefer_from must win over the default prefer_into entity policy"
        );
    }

    #[tokio::test]
    async fn merge_entity_dry_run_previews_append() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
                None,
            )
            .await
            .unwrap();

        assert!(summary.dry_run);
        assert!(
            summary.content_appended,
            "dry-run must preview the append outcome without writing"
        );
        let kept = rt.get_entity(&tok, into.id).await.unwrap();
        assert_eq!(
            kept.description.as_deref(),
            Some("desc A"),
            "dry_run=true must not mutate the into entity's description"
        );
    }

    /// Dry-run must be a read-only, accurate preview: it must predict
    /// `edges_rewired` without writing, and must not append an `EntityMerged` event.
    #[tokio::test]
    async fn merge_entity_dry_run_predicts_edges_rewired_without_writing() {
        use khive_storage::EdgeRelation;

        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();

        rt.link(&tok, a.id, from.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
                None,
            )
            .await
            .unwrap();

        assert!(summary.dry_run);
        assert_eq!(
            summary.edges_rewired, 1,
            "dry-run must predict the edge that would be rewired, not report zero"
        );

        let a_neighbors = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(a_neighbors.len(), 1);
        assert_eq!(
            a_neighbors[0].node_id, from.id,
            "dry_run=true must not rewire any edges"
        );

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::EntityMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(
            events.items.is_empty(),
            "dry_run=true must not append an EntityMerged event"
        );
    }

    /// ADR-014: `reason` is additive — when supplied it must land in the
    /// `EntityMerged` payload verbatim; the key must be entirely absent (not
    /// `null`) when the caller omits it.
    #[tokio::test]
    async fn merge_entity_event_reason_present_when_supplied_absent_when_not() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let into_a = rt
            .create_entity(&tok, "concept", None, "IntoA", None, None, vec![])
            .await
            .unwrap();
        let from_a = rt
            .create_entity(&tok, "concept", None, "FromA", None, None, vec![])
            .await
            .unwrap();
        rt.merge_entity_with_reason(
            &tok,
            into_a.id,
            from_a.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            Some("duplicate".to_string()),
        )
        .await
        .unwrap();

        let into_b = rt
            .create_entity(&tok, "concept", None, "IntoB", None, None, vec![])
            .await
            .unwrap();
        let from_b = rt
            .create_entity(&tok, "concept", None, "FromB", None, None, vec![])
            .await
            .unwrap();
        rt.merge_entity_with_reason(
            &tok,
            into_b.id,
            from_b.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::EntityMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(events.items.len(), 2);

        let with_reason = events
            .items
            .iter()
            .find(|e| {
                e.payload.get("from_id").and_then(|v| v.as_str())
                    == Some(from_a.id.to_string()).as_deref()
            })
            .expect("event for the reasoned merge must exist");
        assert_eq!(
            with_reason.payload.get("reason").and_then(|v| v.as_str()),
            Some("duplicate"),
            "reason must be threaded verbatim into the payload when supplied"
        );

        let without_reason = events
            .items
            .iter()
            .find(|e| {
                e.payload.get("from_id").and_then(|v| v.as_str())
                    == Some(from_b.id.to_string()).as_deref()
            })
            .expect("event for the reasonless merge must exist");
        assert!(
            without_reason.payload.get("reason").is_none(),
            "reason key must be absent (never null) when the caller omits it, got: {:?}",
            without_reason.payload
        );
    }

    #[tokio::test]
    async fn merge_entity_with_reason_preserves_an_explicit_empty_reason() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            Some(String::new()),
        )
        .await
        .unwrap();

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::EntityMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(events.items.len(), 1);
        assert_eq!(
            events.items[0].payload.get("reason"),
            Some(&Value::String(String::new()))
        );
    }

    #[tokio::test]
    async fn merge_entity_with_reason_rejects_secrets_before_reads_or_writes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let secret = secret_shaped_reason();

        let error = rt
            .merge_entity_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                Some(secret),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::SecretDetected(_)));
        assert_eq!(rt.get_entity(&tok, into.id).await.unwrap().id, into.id);
        assert_eq!(rt.get_entity(&tok, from.id).await.unwrap().id, from.id);
        let event_count = rt
            .events(&tok)
            .unwrap()
            .count_events(khive_storage::EventFilter {
                kinds: vec![EventKind::EntityMerged],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(event_count, 0);
    }

    /// ADR-014: `merge_note` must be as auditable as `merge_entity` — exactly one
    /// `NoteMerged` event, carrying kept/absorbed ids, per note merge.
    #[tokio::test]
    async fn merge_note_emits_exactly_one_note_merged_event_with_kept_and_absorbed_ids() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "into note", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "from note", None, None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                Some("duplicate".to_string()),
            )
            .await
            .unwrap();

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::NoteMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            events.items.len(),
            1,
            "merge_note must emit exactly one NoteMerged event"
        );

        let payload = &events.items[0].payload;
        assert_eq!(
            payload.get("into_id").and_then(|v| v.as_str()),
            Some(summary.kept_id.to_string()).as_deref()
        );
        assert_eq!(
            payload.get("from_id").and_then(|v| v.as_str()),
            Some(summary.removed_id.to_string()).as_deref()
        );
        assert_eq!(
            payload.get("reason").and_then(|v| v.as_str()),
            Some("duplicate")
        );
    }

    #[tokio::test]
    async fn merge_note_with_reason_preserves_an_explicit_empty_reason() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "into note", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "from note", None, None, vec![])
            .await
            .unwrap();

        rt.merge_note_with_reason(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            Some(String::new()),
        )
        .await
        .unwrap();

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::NoteMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(events.items.len(), 1);
        assert_eq!(
            events.items[0].payload.get("reason"),
            Some(&Value::String(String::new()))
        );
    }

    #[tokio::test]
    async fn merge_note_with_reason_rejects_secrets_before_reads_or_writes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "into note", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "from note", None, None, vec![])
            .await
            .unwrap();
        let secret = secret_shaped_reason();

        let error = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                Some(secret),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, RuntimeError::SecretDetected(_)));
        let note_store = rt.notes(&tok).unwrap();
        assert_eq!(
            note_store.get_note(into.id).await.unwrap().unwrap().id,
            into.id
        );
        assert_eq!(
            note_store.get_note(from.id).await.unwrap().unwrap().id,
            from.id
        );
        let event_count = rt
            .events(&tok)
            .unwrap()
            .count_events(khive_storage::EventFilter {
                kinds: vec![EventKind::NoteMerged],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(event_count, 0);
    }

    #[tokio::test]
    async fn legacy_merge_methods_remain_source_compatible() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into_entity = rt
            .create_entity(&tok, "concept", None, "Entity A", None, None, vec![])
            .await
            .unwrap();
        let from_entity = rt
            .create_entity(&tok, "concept", None, "Entity B", None, None, vec![])
            .await
            .unwrap();
        let into_note = rt
            .create_note(&tok, "observation", None, "note A", None, None, vec![])
            .await
            .unwrap();
        let from_note = rt
            .create_note(&tok, "observation", None, "note B", None, None, vec![])
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into_entity.id,
            from_entity.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
        rt.merge_note(
            &tok,
            into_note.id,
            from_note.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();
    }

    // ---- interim merged_into miss-hint (data-integrity, precedes ADR-113 chase) ----

    #[tokio::test]
    async fn get_entity_after_merge_discloses_kept_id() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Kept", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "Absorbed", None, None, vec![])
            .await
            .unwrap();

        rt.merge_entity(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .unwrap();

        let err = rt.get_entity(&tok, from.id).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("was merged into") && msg.contains(&into.id.to_string()),
            "expected a merged_into disclosure naming {}, got {msg:?}",
            into.id
        );
    }

    #[tokio::test]
    async fn get_entity_on_plain_soft_delete_stays_bare_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "Deleted", None, None, vec![])
            .await
            .unwrap();
        assert!(rt.delete_entity(&tok, entity.id, false).await.unwrap());

        let err = rt.get_entity(&tok, entity.id).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("merged into"),
            "plain soft-delete must not gain a merge hint, got {msg:?}"
        );
        assert_eq!(msg, format!("not found: entity {}", entity.id));
    }

    #[tokio::test]
    async fn get_entity_on_absent_id_stays_bare_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let absent = Uuid::new_v4();

        let err = rt.get_entity(&tok, absent).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("merged into"),
            "a never-existed id must not gain a merge hint, got {msg:?}"
        );
        assert_eq!(msg, format!("not found: entity {absent}"));
    }

    // ---- merge helper unit tests ----

    #[test]
    fn union_tags_deduplicates() {
        let (tags, added) = union_tags(
            &["x".to_string(), "y".to_string()],
            &["y".to_string(), "z".to_string()],
        );
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["x", "y", "z"]);
        assert_eq!(added, 1);
    }

    #[test]
    fn merge_properties_prefer_into_fills_missing_keys() {
        let a = serde_json::json!({"a": 1});
        let b = serde_json::json!({"a": 99, "b": 2});
        let (merged, added) =
            merge_properties(&Some(a), &Some(b), EntityDedupMergePolicy::PreferInto);
        let m = merged.unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"], 2);
        assert_eq!(added, 1);
    }

    // ---- tombstone and note merge tests ----

    #[tokio::test]
    async fn merge_entity_tombstones_source_with_provenance() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let from_id = from.id;

        rt.merge_entity_with_reason(
            &tok,
            into.id,
            from_id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();

        assert!(
            rt.get_entity(&tok, from_id).await.is_err(),
            "tombstoned source should not be returned by get_entity"
        );

        let pool = rt.backend().pool_arc();
        let (deleted_at, merged_into): (Option<i64>, Option<String>) =
            tokio::task::spawn_blocking(move || {
                let guard = pool.writer().unwrap();
                guard
                    .conn()
                    .query_row(
                        "SELECT deleted_at, merged_into FROM entities WHERE id = ?1",
                        [from_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap()
            })
            .await
            .unwrap();
        assert!(
            deleted_at.is_some(),
            "tombstoned entity must have deleted_at set"
        );
        assert_eq!(
            merged_into.as_deref(),
            Some(into.id.to_string().as_str()),
            "merged_into must point to into_id"
        );
    }

    #[tokio::test]
    async fn generic_update_and_merge_reject_schedule_managed_notes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let schedule_a = rt
            .create_note(
                &tok,
                "scheduled_event",
                None,
                "stats()",
                None,
                Some(serde_json::json!({
                    "event_type": "schedule",
                    "payload": "stats()",
                    "status": "pending",
                    "trigger_at": "2099-01-01T00:00:00Z"
                })),
                vec![],
            )
            .await
            .unwrap();
        let schedule_b = rt
            .create_note(
                &tok,
                "scheduled_event",
                None,
                "stats()",
                None,
                Some(serde_json::json!({
                    "event_type": "schedule",
                    "payload": "stats()",
                    "status": "pending",
                    "trigger_at": "2099-01-02T00:00:00Z"
                })),
                vec![],
            )
            .await
            .unwrap();

        let update_error = rt
            .update_note(
                &tok,
                schedule_a.id,
                NotePatch::new(
                    None,
                    None,
                    None,
                    None,
                    Some(serde_json::json!({ "payload": "delete(id=\"victim\")" })),
                ),
            )
            .await
            .expect_err("schedule-managed note update must fail");
        assert!(
            update_error.to_string().contains("schedule-managed"),
            "{update_error}"
        );

        for (into_id, from_id) in [
            (schedule_a.id, schedule_b.id),
            (schedule_b.id, schedule_a.id),
        ] {
            let merge_error = rt
                .merge_note(
                    &tok,
                    into_id,
                    from_id,
                    EntityDedupMergePolicy::PreferFrom,
                    ContentMergeStrategy::PreferFrom,
                    false,
                )
                .await
                .expect_err("either schedule-managed merge operand must fail");
            assert!(
                merge_error.to_string().contains("schedule-managed"),
                "{merge_error}"
            );
        }

        let store = rt.notes(&tok).unwrap();
        for (id, trigger_at) in [
            (schedule_a.id, "2099-01-01T00:00:00Z"),
            (schedule_b.id, "2099-01-02T00:00:00Z"),
        ] {
            let note = store
                .get_note(id)
                .await
                .unwrap()
                .expect("rejected generic mutation leaves the schedule intact");
            assert_eq!(note.properties.as_ref().unwrap()["trigger_at"], trigger_at);
        }
    }

    #[tokio::test]
    async fn merge_note_refuses_quarantined_message_in_either_role() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let capability = crate::pack::ChannelIngestCapability { _sealed: () };
        let ordinary = rt
            .create_note(
                &tok,
                "message",
                None,
                "ordinary message",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let quarantined = rt
            .try_create_note_as_trusted_ingest(
                &capability,
                &tok,
                "message",
                None,
                "quarantined transport content",
                Some(serde_json::json!({"quarantined": true})),
            )
            .await
            .unwrap()
            .expect("quarantined insert");

        for (into_id, from_id) in [(ordinary.id, quarantined.id), (quarantined.id, ordinary.id)] {
            let error = rt
                .merge_note(
                    &tok,
                    into_id,
                    from_id,
                    EntityDedupMergePolicy::PreferFrom,
                    ContentMergeStrategy::Append,
                    false,
                )
                .await
                .expect_err("a quarantined message must not merge in either role");
            assert!(error.to_string().contains("quarantined"), "{error}");
        }

        // Neither operand was mutated by the refused merges.
        let store = rt.notes(&tok).unwrap();
        let kept = store
            .get_note(quarantined.id)
            .await
            .unwrap()
            .expect("quarantined note intact");
        assert_eq!(
            kept.properties.as_ref().unwrap()["quarantined"],
            serde_json::json!(true)
        );
        assert_eq!(kept.content, "quarantined transport content");
    }

    #[tokio::test]
    async fn merge_note_refuses_string_encoded_quarantine_marker() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let capability = crate::pack::ChannelIngestCapability { _sealed: () };
        let ordinary = rt
            .create_note(
                &tok,
                "message",
                None,
                "ordinary message",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        // Some channel adapters record the marker as the string "true".
        let quarantined = rt
            .try_create_note_as_trusted_ingest(
                &capability,
                &tok,
                "message",
                None,
                "string-marked quarantined content",
                Some(serde_json::json!({"quarantined": "true"})),
            )
            .await
            .unwrap()
            .expect("quarantined insert");

        let error = rt
            .merge_note(
                &tok,
                ordinary.id,
                quarantined.id,
                EntityDedupMergePolicy::PreferFrom,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .expect_err("string-encoded quarantine marker must also refuse the merge");
        assert!(error.to_string().contains("quarantined"), "{error}");
    }

    #[tokio::test]
    async fn merge_note_still_merges_unquarantined_messages() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "message", None, "into message", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "message", None, "from message", None, None, vec![])
            .await
            .unwrap();
        rt.merge_note(
            &tok,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .expect("ordinary message merge must still work");
    }

    #[tokio::test]
    async fn merge_note_same_kind_appends_content() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "Into content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_note(
                &tok,
                "observation",
                None,
                "From content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from_id = from.id;

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(summary.kept_id, into.id);
        assert_eq!(summary.removed_id, from_id);
        assert!(summary.content_appended);
        assert!(!summary.dry_run);

        let from_store = rt.notes(&tok).unwrap();
        assert!(
            from_store.get_note(from_id).await.unwrap().is_none(),
            "merged-from note should be soft-deleted"
        );
    }

    // Note merge must absorb a conflicting edge natural key exactly like entity
    // merge does, since both route through the shared EDGE_SYMMETRIC_*_SQL arms.
    #[tokio::test]
    async fn merge_note_survives_shared_edge_to_third_party() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        // Both into and from annotate the same shared entity — rewiring from's
        // edge onto into during merge produces a duplicate (into, shared,
        // annotates) triple, exercising the conflict-probe/delete arms.
        rt.link(&tok, into.id, shared.id, EdgeRelation::Annotates, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, from.id, shared.id, EdgeRelation::Annotates, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .expect("merge must succeed even when both notes annotate the same entity");

        assert_eq!(summary.kept_id, into.id);
        assert_eq!(summary.removed_id, from.id);

        let into_edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(into.id),
                    target_id: Some(shared.id),
                    relations: vec![EdgeRelation::Annotates],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            into_edges.len(),
            1,
            "exactly one live into→shared annotates edge must exist after merge; got: {into_edges:?}"
        );
    }

    #[tokio::test]
    async fn merge_note_conflict_records_dropped_edge_and_cascades_annotation() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();
        let annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "edge annotation",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        let survivor = rt
            .link(
                &tok,
                into.id,
                shared.id,
                EdgeRelation::Annotates,
                1.0,
                Some(serde_json::json!({"source": "survivor"})),
            )
            .await
            .unwrap();
        let dropped = rt
            .link(
                &tok,
                from.id,
                shared.id,
                EdgeRelation::Annotates,
                0.4,
                Some(serde_json::json!({"source": "dropped"})),
            )
            .await
            .unwrap();
        let annotation = rt
            .link(
                &tok,
                annotator.id,
                dropped.id.into(),
                EdgeRelation::Annotates,
                0.8,
                Some(serde_json::json!({"why": "duplicate claim"})),
            )
            .await
            .unwrap();
        rt.delete_edge(&tok, annotation.id.into(), false)
            .await
            .unwrap();

        let summary = rt
            .merge_note(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .unwrap();

        let [conflict] = summary.edge_conflict_preimages.as_slice() else {
            panic!(
                "expected one note-merge edge conflict, got {:?}",
                summary.edge_conflict_preimages
            );
        };
        assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
        assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
        assert_eq!(conflict.dropped_edge.source_id, from.id);
        assert_eq!(conflict.dropped_edge.weight, 0.4);
        assert_eq!(
            conflict.dropped_edge.metadata,
            Some(serde_json::json!({"source": "dropped"}))
        );
        assert_eq!(conflict.incident_edge_preimages.len(), 1);
        assert_eq!(
            conflict.incident_edge_preimages[0].id,
            Uuid::from(annotation.id)
        );
        assert_eq!(
            conflict.incident_edge_preimages[0].metadata,
            Some(serde_json::json!({"why": "duplicate claim"}))
        );
        assert!(
            conflict.incident_edge_preimages[0].deleted_at.is_some(),
            "the cascade preimage must retain an annotation's tombstone state"
        );
        assert!(rt
            .get_edge_including_deleted(&tok, dropped.id.into())
            .await
            .unwrap()
            .is_none());
        assert!(
            rt.get_edge_including_deleted(&tok, annotation.id.into())
                .await
                .unwrap()
                .is_none(),
            "annotation targeting the dropped edge must be cascaded, not left dangling"
        );

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::NoteMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(events.items.len(), 1);
        assert_eq!(
            events.items[0].payload["edge_conflict_preimages"],
            serde_json::to_value(&summary.edge_conflict_preimages).unwrap()
        );
    }

    // A dry run must predict the same conflict preimages a committing note
    // merge would produce, without deleting or mutating a single row. The
    // incident cascade is two levels deep (an annotation on the dropped
    // edge, and a nested annotation on that annotation) so the root-to-leaf
    // ordering ADR-014 promises is actually exercised, not just a
    // one-element vec that trivially satisfies any order. Every row touched
    // by the merge — both notes and every edge — is snapshotted before the
    // dry run and compared field-for-field against its post-run state.
    #[tokio::test]
    async fn merge_note_dry_run_conflict_returns_preimages_without_mutating() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();
        let annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "edge annotation",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let nested_annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "nested edge annotation",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        let survivor = rt
            .link(
                &tok,
                into.id,
                shared.id,
                EdgeRelation::Annotates,
                1.0,
                Some(serde_json::json!({"source": "survivor"})),
            )
            .await
            .unwrap();
        let dropped = rt
            .link(
                &tok,
                from.id,
                shared.id,
                EdgeRelation::Annotates,
                0.4,
                Some(serde_json::json!({"source": "dropped"})),
            )
            .await
            .unwrap();
        let annotation = rt
            .link(
                &tok,
                annotator.id,
                dropped.id.into(),
                EdgeRelation::Annotates,
                0.8,
                Some(serde_json::json!({"why": "duplicate claim"})),
            )
            .await
            .unwrap();
        let nested_annotation = rt
            .link(
                &tok,
                nested_annotator.id,
                annotation.id.into(),
                EdgeRelation::Annotates,
                0.6,
                Some(serde_json::json!({"why": "nested duplicate claim"})),
            )
            .await
            .unwrap();
        rt.delete_edge(&tok, nested_annotation.id.into(), false)
            .await
            .unwrap();

        let survivor_before = rt
            .get_edge_including_deleted(&tok, survivor.id.into())
            .await
            .unwrap()
            .expect("survivor edge exists");
        let dropped_before = rt
            .get_edge_including_deleted(&tok, dropped.id.into())
            .await
            .unwrap()
            .expect("dropped edge exists");
        let annotation_before = rt
            .get_edge_including_deleted(&tok, annotation.id.into())
            .await
            .unwrap()
            .expect("annotation edge exists");
        let nested_annotation_before = rt
            .get_edge_including_deleted(&tok, nested_annotation.id.into())
            .await
            .unwrap()
            .expect("nested annotation edge exists");
        let into_before = rt
            .get_note_including_deleted(&tok, into.id)
            .await
            .unwrap()
            .expect("into note exists");
        let from_before = rt
            .get_note_including_deleted(&tok, from.id)
            .await
            .unwrap()
            .expect("from note exists");

        let summary = rt
            .merge_note(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
            )
            .await
            .unwrap();

        let [conflict] = summary.edge_conflict_preimages.as_slice() else {
            panic!(
                "expected one note-merge edge conflict from the dry run, got {:?}",
                summary.edge_conflict_preimages
            );
        };
        assert_eq!(conflict.surviving_edge_id, Uuid::from(survivor.id));
        assert_eq!(conflict.dropped_edge.id, Uuid::from(dropped.id));
        assert_eq!(conflict.dropped_edge.source_id, from.id);
        assert_eq!(conflict.dropped_edge.weight, 0.4);
        // Root-to-leaf order (ADR-014): the direct annotation on the dropped
        // edge must precede the annotation nested on top of it.
        assert_eq!(conflict.incident_edge_preimages.len(), 2);
        assert_eq!(
            conflict.incident_edge_preimages[0].id,
            Uuid::from(annotation.id)
        );
        assert!(
            conflict.incident_edge_preimages[0].deleted_at.is_none(),
            "the direct annotation was never soft-deleted"
        );
        assert_eq!(
            conflict.incident_edge_preimages[1].id,
            Uuid::from(nested_annotation.id)
        );
        assert!(
            conflict.incident_edge_preimages[1].deleted_at.is_some(),
            "dry-run preimage must retain the nested annotation's tombstone state"
        );

        let survivor_after = rt
            .get_edge_including_deleted(&tok, survivor.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the survivor edge");
        let dropped_after = rt
            .get_edge_including_deleted(&tok, dropped.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the dropped edge");
        let annotation_after = rt
            .get_edge_including_deleted(&tok, annotation.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the cascaded annotation");
        let nested_annotation_after = rt
            .get_edge_including_deleted(&tok, nested_annotation.id.into())
            .await
            .unwrap()
            .expect("dry run must not delete the nested cascaded annotation");
        assert_eq!(
            serde_json::to_value(&survivor_before).unwrap(),
            serde_json::to_value(&survivor_after).unwrap(),
            "dry run must not mutate the surviving edge's row at all"
        );
        assert_eq!(
            serde_json::to_value(&dropped_before).unwrap(),
            serde_json::to_value(&dropped_after).unwrap(),
            "dry run must not mutate the would-be-dropped edge's row at all"
        );
        assert_eq!(
            serde_json::to_value(&annotation_before).unwrap(),
            serde_json::to_value(&annotation_after).unwrap(),
            "dry run must not mutate the incident annotation's row at all"
        );
        assert_eq!(
            serde_json::to_value(&nested_annotation_before).unwrap(),
            serde_json::to_value(&nested_annotation_after).unwrap(),
            "dry run must not mutate the nested incident annotation's row at all"
        );

        let into_after = rt
            .get_note_including_deleted(&tok, into.id)
            .await
            .unwrap()
            .expect("into note must remain unmerged after a dry run");
        let from_after = rt
            .get_note_including_deleted(&tok, from.id)
            .await
            .unwrap()
            .expect("from note must not be deleted by a dry run");
        assert_eq!(
            serde_json::to_value(&into_before).unwrap(),
            serde_json::to_value(&into_after).unwrap(),
            "dry run must not mutate the into note's row at all"
        );
        assert_eq!(
            serde_json::to_value(&from_before).unwrap(),
            serde_json::to_value(&from_after).unwrap(),
            "dry run must not mutate the from note's row at all"
        );
        assert_eq!(from_after.status, from_before.status);
        assert_eq!(from_after.deleted_at, None);

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::NoteMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(
            events.items.is_empty(),
            "a dry run must not record a merge audit event"
        );
    }

    // The rewire contract check must preserve note→note supersedes, supports,
    // and refutes — `validate_edge_relation_endpoints` permits any note→note
    // pair for these relations, so the merge matcher must too, or a note merge
    // deletes valid epistemic/supersession edges.
    #[tokio::test]
    async fn merge_note_preserves_note_to_note_epistemic_and_supersession_edges() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();
        let superseded = rt
            .create_note(&tok, "observation", None, "Old", None, None, vec![])
            .await
            .unwrap();
        let claim = rt
            .create_note(&tok, "insight", None, "Claim", None, None, vec![])
            .await
            .unwrap();
        let counter = rt
            .create_note(&tok, "observation", None, "Counter", None, None, vec![])
            .await
            .unwrap();

        // Outgoing from `from` (source rewires) and incoming onto `from`
        // (target rewires) — both directions must survive.
        rt.link(
            &tok,
            from.id,
            superseded.id,
            EdgeRelation::Supersedes,
            1.0,
            None,
        )
        .await
        .unwrap();
        rt.link(&tok, from.id, claim.id, EdgeRelation::Supports, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, counter.id, from.id, EdgeRelation::Refutes, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 3,
            "all three note→note edges must be rewired, not contract-dropped"
        );
        assert_eq!(
            summary.edges_contract_skipped, 0,
            "no valid note→note supersedes/supports/refutes edge may be dropped"
        );

        for (src, tgt, rel) in [
            (into.id, superseded.id, EdgeRelation::Supersedes),
            (into.id, claim.id, EdgeRelation::Supports),
            (counter.id, into.id, EdgeRelation::Refutes),
        ] {
            let edges = rt
                .list_edges(
                    &tok,
                    crate::EdgeListFilter {
                        source_id: Some(src),
                        target_id: Some(tgt),
                        relations: vec![rel],
                        ..Default::default()
                    },
                    10,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(
                edges.len(),
                1,
                "rewired {rel:?} edge {src}→{tgt} must survive the merge; got {edges:?}"
            );
        }
    }

    // Annotates targets may be edges or events — substrates
    // `resolve_merge_edge_endpoint` cannot resolve. The contract check must
    // exempt annotates BEFORE endpoint resolution, or a note merge deletes
    // valid annotates edges pointing at them.
    #[tokio::test]
    async fn merge_note_preserves_annotates_edges_targeting_edges_and_events() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        // An edge to annotate.
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let annotated_edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // An event to annotate: a throwaway note merge emits a NoteMerged
        // event (creation ops don't emit in this harness).
        let scrap_into = rt
            .create_note(&tok, "observation", None, "ScrapInto", None, None, vec![])
            .await
            .unwrap();
        let scrap_from = rt
            .create_note(&tok, "observation", None, "ScrapFrom", None, None, vec![])
            .await
            .unwrap();
        rt.merge_note_with_reason(
            &tok,
            scrap_into.id,
            scrap_from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .unwrap();
        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::NoteMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .unwrap();
        let annotated_event_id = events.items[0].id;

        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();

        rt.link(
            &tok,
            from.id,
            annotated_edge.id.0,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();
        rt.link(
            &tok,
            from.id,
            annotated_event_id,
            EdgeRelation::Annotates,
            1.0,
            None,
        )
        .await
        .unwrap();

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            summary.edges_rewired, 2,
            "annotates edges targeting an edge and an event must be rewired"
        );
        assert_eq!(
            summary.edges_contract_skipped, 0,
            "no valid annotates edge may be dropped as contract-violating"
        );

        for tgt in [annotated_edge.id.0, annotated_event_id] {
            let edges = rt
                .list_edges(
                    &tok,
                    crate::EdgeListFilter {
                        source_id: Some(into.id),
                        target_id: Some(tgt),
                        relations: vec![EdgeRelation::Annotates],
                        ..Default::default()
                    },
                    10,
                    0,
                )
                .await
                .unwrap();
            assert_eq!(
                edges.len(),
                1,
                "rewired annotates edge onto target {tgt} must survive the merge; got {edges:?}"
            );
        }
    }

    // A note dry-run must predict edges_rewired like the entity path does,
    // and must not touch topology.
    #[tokio::test]
    async fn merge_note_dry_run_predicts_edges_rewired() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, from.id, shared.id, EdgeRelation::Annotates, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
                None,
            )
            .await
            .unwrap();
        assert!(summary.dry_run);
        assert_eq!(
            summary.edges_rewired, 1,
            "note dry-run must predict the rewire count"
        );

        let from_edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(from.id),
                    target_id: Some(shared.id),
                    relations: vec![EdgeRelation::Annotates],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(from_edges.len(), 1, "dry-run must leave topology untouched");
    }

    #[tokio::test]
    async fn merge_note_different_kinds_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "decision", None, "From", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .merge_note_with_reason(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await;
        assert!(result.is_err(), "merging different note kinds must fail");
    }

    #[tokio::test]
    async fn merge_note_dry_run_leaves_notes_unchanged() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "Into content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_note(
                &tok,
                "observation",
                None,
                "From content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let into_id = into.id;
        let from_id = from.id;

        let summary = rt
            .merge_note_with_reason(
                &tok,
                into_id,
                from_id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
                None,
            )
            .await
            .unwrap();

        assert!(summary.dry_run);

        let store = rt.notes(&tok).unwrap();
        let into_after = store.get_note(into_id).await.unwrap().unwrap();
        let from_after = store.get_note(from_id).await.unwrap().unwrap();
        assert_eq!(
            into_after.content, "Into content",
            "dry_run must not mutate into-note"
        );
        assert_eq!(
            from_after.content, "From content",
            "dry_run must not mutate from-note"
        );

        let events = rt
            .events(&tok)
            .unwrap()
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![EventKind::NoteMerged],
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert!(
            events.items.is_empty(),
            "dry_run=true must not append a NoteMerged event"
        );
    }

    // Merging two nameless notes with no embedding model configured: a raw SQL FTS
    // INSERT binding &merged_name directly would store SQL NULL for a nameless
    // note, while Fts5TextSearch::upsert_document stores an empty string:
    // note_fts_scalars must keep the round-trip field-identical.
    #[tokio::test]
    async fn merge_nameless_notes_fts_document_is_parity_correct() {
        use khive_storage::types::TextSearchRequest;

        let rt = rt(); // in-memory runtime — no embedding model configured
        let tok = NamespaceToken::local();

        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "intosentinelzxq body",
                None,
                Some(serde_json::json!({"src": "into"})),
                vec![],
            )
            .await
            .expect("create into-note");
        let from = rt
            .create_note(
                &tok,
                "observation",
                None,
                "fromsentinelzxq body",
                None,
                None,
                vec![],
            )
            .await
            .expect("create from-note");

        let into_id = into.id;
        let from_id = from.id;

        rt.merge_note_with_reason(
            &tok,
            into_id,
            from_id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("merge_note must succeed");

        let note_store = rt.notes(&tok).expect("note store");
        let merged_note = note_store
            .get_note(into_id)
            .await
            .expect("get_note")
            .expect("merged note must exist");

        let expected = note_fts_document(&merged_note);

        let fts = rt.text_for_notes(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", into_id)
            .await
            .expect("get_document must not error")
            .expect("FTS document must exist after merge");

        assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
        assert_eq!(
            stored.title, expected.title,
            "title (None for nameless note)"
        );
        assert_eq!(stored.body, expected.body, "body");
        assert_eq!(stored.namespace, expected.namespace, "namespace");
        assert_eq!(stored.kind, expected.kind, "kind");

        assert!(
            stored.title.is_none(),
            "nameless merged note must have title=None in FTS (was NULL before fix)"
        );

        // The merged note must be searchable by a unique token from the into-note body.
        let hits = fts
            .search(TextSearchRequest {
                query: "intosentinelzxq".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: None,
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .expect("search");
        assert!(
            hits.iter().any(|h| h.subject_id == into_id),
            "merged note must be searchable by into-note content"
        );
    }

    #[tokio::test]
    async fn update_edge_updates_properties() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.5, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                EdgePatch {
                    properties: Some(serde_json::json!({"source": "manual"})),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.metadata.as_ref().unwrap()["source"], "manual");
        assert!((updated.weight - 0.5).abs() < 0.001, "weight unchanged");
    }

    // Merge must not crash when both entities share a common third-party edge
    // (duplicate triple after rewire): a double-ON-CONFLICT INSERT would
    // otherwise raise a UNIQUE constraint error and abort mid-transaction.
    #[tokio::test]
    async fn merge_entity_survives_shared_edge_to_third_party() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        // A and B will be merged; shared is the common target. `extends` is used
        // since concept→concept is a valid endpoint combination.
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        // Both A and B extend the same shared concept — this creates a duplicate
        // triple (A/B → shared, extends) that triggers the crash on rewire.
        rt.link(&tok, a.id, shared.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, b.id, shared.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                a.id,
                b.id,
                crate::EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .expect(
                "C1: merge must succeed even when both entities share an edge to a third party",
            );

        assert_eq!(summary.kept_id, a.id);
        assert_eq!(summary.removed_id, b.id);
        // A already had the Extends edge to shared; rewiring B->shared onto it
        // hits the natural-key conflict arm, which drops the incoming (B-side)
        // duplicate rather than erroring or touching A's surviving row (ADR-039
        // `ON CONFLICT ... DO NOTHING`). The invariant checked below is that
        // exactly one live edge A->shared remains.
        let a_edges = rt
            .list_edges(
                &tok,
                crate::EdgeListFilter {
                    source_id: Some(a.id),
                    target_id: Some(shared.id),
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            a_edges.len(),
            1,
            "C1: exactly one live A→shared Extends edge must exist after merge; got: {a_edges:?}"
        );

        // get_entity filters deleted_at IS NULL, so a tombstoned entity returns None.
        let b_after = rt.entities(&tok).unwrap().get_entity(b.id).await.unwrap();
        assert!(
            b_after.is_none(),
            "C3: from_entity must be tombstoned (get_entity returns None for deleted) after merge; got: {b_after:?}"
        );
    }

    // ADR-039 conflict-arm regression (#1191): on a symmetric-edge merge collision,
    // the surviving row's own weight/metadata must never be overwritten with the
    // incoming (dropped) duplicate's values.
    #[tokio::test]
    async fn merge_entity_symmetric_conflict_preserves_survivor_fields() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        let survivor_edge = rt
            .link(
                &tok,
                a.id,
                shared.id,
                EdgeRelation::Extends,
                1.0,
                Some(serde_json::json!({"source": "survivor"})),
            )
            .await
            .unwrap();
        rt.link(
            &tok,
            b.id,
            shared.id,
            EdgeRelation::Extends,
            0.3,
            Some(serde_json::json!({"source": "loser"})),
        )
        .await
        .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            a.id,
            b.id,
            crate::EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("merge must succeed across the symmetric-edge collision");

        let after = rt
            .get_edge(&tok, survivor_edge.id.into())
            .await
            .unwrap()
            .expect("survivor edge must still exist after merge");
        assert!(
            (after.weight - 1.0).abs() < 0.001,
            "survivor weight must be untouched by the dropped duplicate; got {}",
            after.weight
        );
        assert_eq!(
            after.metadata.as_ref().unwrap()["source"],
            "survivor",
            "survivor metadata must be untouched by the dropped duplicate; got {:?}",
            after.metadata
        );
    }

    // ADR-039 conflict-arm regression (#1191): a soft-deleted survivor row must
    // stay soft-deleted after a merge collision, never resurrected.
    #[tokio::test]
    async fn merge_entity_symmetric_conflict_does_not_resurrect_soft_deleted_survivor() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();

        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();

        let survivor_edge = rt
            .link(&tok, a.id, shared.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.delete_edge(&tok, survivor_edge.id.into(), false)
            .await
            .unwrap();
        rt.link(&tok, b.id, shared.id, EdgeRelation::Extends, 0.5, None)
            .await
            .unwrap();

        rt.merge_entity_with_reason(
            &tok,
            a.id,
            b.id,
            crate::EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("merge must succeed even when the surviving edge is soft-deleted");

        let after = rt
            .get_edge_including_deleted(&tok, survivor_edge.id.into())
            .await
            .unwrap()
            .expect("survivor edge row must still exist after merge");
        assert!(
            after.deleted_at.is_some(),
            "soft-deleted survivor must stay soft-deleted after merge collision; got: {after:?}"
        );
    }

    // merge_entity at the runtime level must reject cross-kind merges: without this
    // guard, a direct runtime caller could merge concept+project, silently
    // tombstoning the source entity, even though the pack handler also checks it.
    #[tokio::test]
    async fn merge_entity_cross_kind_rejected_at_runtime() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let concept = rt
            .create_entity(&tok, "concept", None, "H2Concept", None, None, vec![])
            .await
            .unwrap();
        let project = rt
            .create_entity(&tok, "project", None, "H2Project", None, None, vec![])
            .await
            .unwrap();

        let err = rt
            .merge_entity_with_reason(
                &tok,
                concept.id,
                project.id,
                crate::EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .expect_err("H2: cross-kind merge must be rejected by runtime");
        assert!(
            matches!(err, crate::RuntimeError::InvalidInput(_)),
            "H2: expected InvalidInput, got: {err:?}"
        );

        let concept_after = rt.get_entity(&tok, concept.id).await;
        let project_after = rt.get_entity(&tok, project.id).await;
        assert!(
            concept_after.is_ok(),
            "H2: concept must remain live after rejected merge; got: {concept_after:?}"
        );
        assert!(
            project_after.is_ok(),
            "H2: project must remain live after rejected merge; got: {project_after:?}"
        );
    }

    // Same-kind merge must succeed.
    #[tokio::test]
    async fn merge_entity_same_kind_succeeds() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let c1 = rt
            .create_entity(&tok, "concept", None, "Concept1", None, None, vec![])
            .await
            .unwrap();
        let c2 = rt
            .create_entity(&tok, "concept", None, "Concept2", None, None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                c1.id,
                c2.id,
                crate::EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .expect("same-kind merge must succeed");
        assert_eq!(summary.kept_id, c1.id);
        assert_eq!(summary.removed_id, c2.id);

        let c2_after = rt.entities(&tok).unwrap().get_entity(c2.id).await.unwrap();
        assert!(c2_after.is_none(), "from_entity must be tombstoned");
    }

    #[tokio::test]
    async fn merge_entity_explicit_policy_rereads_names_before_commit() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let into = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Transactional Guard",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let from = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "Transactional Guard",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        validate_entity_merge_floor(&into, &from)
            .expect("the handler's fast-path validation would initially pass");
        let renamed_into = rt
            .update_entity(
                &tok,
                into.id,
                EntityPatch {
                    name: Some("Unrelated Renamed Target".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let expected = validate_entity_merge_floor(&renamed_into, &from)
            .expect_err("the renamed transactional state must violate the name guard");
        let RuntimeError::Khive(expected) = entity_merge_guard_error(expected) else {
            unreachable!("merge guard errors are structured Khive errors")
        };

        let err = rt
            .merge_entity_with_reason_and_force(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
                false,
            )
            .await
            .expect_err("the explicit non-forced path must validate its transactional reread");
        let RuntimeError::Khive(err) = err else {
            panic!("expected a structured merge-guard conflict, got {err:?}");
        };
        assert_eq!(err.kind(), expected.kind());
        assert_eq!(err.message(), expected.message());
        assert_eq!(err.code(), expected.code());
        assert_eq!(err.details(), expected.details());
        assert!(
            rt.get_entity(&tok, from.id).await.is_ok(),
            "a refused merge must leave the source entity live"
        );
    }

    // Cross-namespace merge_note must be denied on either ID.

    #[tokio::test]
    async fn merge_note_cross_namespace_either_id_returns_not_found() {
        use crate::error::RuntimeError;
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let into_a = rt
            .create_note(&ns_a, "observation", None, "Into A", None, None, vec![])
            .await
            .unwrap();
        let from_a = rt
            .create_note(&ns_a, "observation", None, "From A", None, None, vec![])
            .await
            .unwrap();
        let note_b = rt
            .create_note(&ns_b, "observation", None, "Note B", None, None, vec![])
            .await
            .unwrap();

        // foreign into_id: note_b belongs to ns_b, caller token is ns_a
        let foreign_into = rt
            .merge_note_with_reason(
                &ns_a,
                note_b.id,
                from_a.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await;
        assert!(
            matches!(foreign_into, Err(RuntimeError::NotFound(_))),
            "foreign into_id must be denied before merge, got {foreign_into:?}"
        );

        // foreign from_id: note_b belongs to ns_b, caller token is ns_a
        let foreign_from = rt
            .merge_note_with_reason(
                &ns_a,
                into_a.id,
                note_b.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await;
        assert!(
            matches!(foreign_from, Err(RuntimeError::NotFound(_))),
            "foreign from_id must be denied before merge, got {foreign_from:?}"
        );
    }

    // Cross-namespace update now succeeds (shared-brain model).

    #[tokio::test]
    async fn update_entity_cross_namespace_succeeds() {
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let entity = rt
            .create_entity(
                &ns_a,
                "concept",
                None,
                "Alpha",
                Some("original"),
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .update_entity(
                &ns_b,
                entity.id,
                EntityPatch {
                    name: Some("Updated".into()),
                    ..Default::default()
                },
            )
            .await;

        assert!(
            result.is_ok(),
            "cross-namespace update must succeed in shared-brain OSS; got {result:?}"
        );
        assert_eq!(result.unwrap().name, "Updated");
    }

    // merge_entity still requires both entities to be in the same namespace as
    // the token's write namespace (enforced at the SQL transaction layer, not the
    // runtime layer).  This is a merge-semantic constraint, not tenant isolation.
    #[tokio::test]
    async fn merge_entity_cross_namespace_ids_fail_at_sql_layer() {
        use crate::Namespace;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let into_a = rt
            .create_entity(&ns_a, "concept", None, "Into A", None, None, vec![])
            .await
            .unwrap();
        let from_a = rt
            .create_entity(&ns_a, "concept", None, "From A", None, None, vec![])
            .await
            .unwrap();
        let foreign_b = rt
            .create_entity(&ns_b, "concept", None, "Foreign B", None, None, vec![])
            .await
            .unwrap();

        // foreign into_id: SQL read_merge_entity checks ns matches token namespace.
        let foreign_into = rt
            .merge_entity_with_reason(
                &ns_a,
                foreign_b.id,
                from_a.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await;
        assert!(
            foreign_into.is_err(),
            "cross-namespace into_id must still fail at SQL layer; got {foreign_into:?}"
        );

        // foreign from_id: same SQL constraint.
        let foreign_from = rt
            .merge_entity_with_reason(
                &ns_a,
                into_a.id,
                foreign_b.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await;
        assert!(
            foreign_from.is_err(),
            "cross-namespace from_id must still fail at SQL layer; got {foreign_from:?}"
        );

        // All three entities survive the failed merges.
        assert!(rt.get_entity(&ns_a, into_a.id).await.is_ok());
        assert!(rt.get_entity(&ns_a, from_a.id).await.is_ok());
        assert!(rt.get_entity(&ns_b, foreign_b.id).await.is_ok());
    }

    // Parity: entity_fts_document must produce the same body/title as the
    // create_entity and update_entity FTS write paths.
    #[test]
    fn entity_fts_document_with_description() {
        let mut entity = Entity::new("local", "concept", "MyEntity");
        entity = entity.with_description("some description text");
        let doc = entity_fts_document(&entity);
        assert_eq!(doc.subject_id, entity.id);
        assert_eq!(doc.namespace, "local");
        assert_eq!(doc.title.as_deref(), Some("MyEntity"));
        assert_eq!(doc.body, "MyEntity some description text");
        assert_eq!(doc.kind, khive_types::SubstrateKind::Entity);
    }

    #[test]
    fn entity_fts_document_without_description() {
        let entity = Entity::new("local", "concept", "NameOnly");
        let doc = entity_fts_document(&entity);
        assert_eq!(doc.title.as_deref(), Some("NameOnly"));
        assert_eq!(doc.body, "NameOnly");
    }

    #[test]
    fn entity_fts_document_empty_description_uses_name_only() {
        let mut entity = Entity::new("local", "concept", "TitleOnly");
        entity = entity.with_description("");
        let doc = entity_fts_document(&entity);
        assert_eq!(
            doc.body, "TitleOnly",
            "empty description must not be appended"
        );
    }

    // Cross-path equality: an entity created through the runtime (operations.rs
    // create_entity path) must produce a stored FTS document field-identical to
    // entity_fts_document() called on the same Entity.
    #[tokio::test]
    async fn entity_fts_document_matches_runtime_create_path() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "CrossPathTitle",
                Some("cross path description body"),
                Some(serde_json::json!({"key": "val"})),
                vec!["tag1".to_string()],
            )
            .await
            .expect("create_entity");

        let fts = rt.text(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", entity.id)
            .await
            .expect("get_document")
            .expect("document must exist after create_entity");

        let expected = entity_fts_document(&entity);

        assert_eq!(stored.subject_id, expected.subject_id, "subject_id");
        assert_eq!(stored.kind, expected.kind, "kind");
        assert_eq!(stored.title, expected.title, "title");
        assert_eq!(stored.body, expected.body, "body");
        assert_eq!(stored.namespace, expected.namespace, "namespace");
    }

    // Cross-path equality: update_entity must produce a stored FTS document
    // field-identical to entity_fts_document() on the updated Entity.
    #[tokio::test]
    async fn entity_fts_document_matches_runtime_update_path() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "OldName",
                Some("old desc"),
                None,
                vec![],
            )
            .await
            .expect("create_entity");

        let updated = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    name: Some("NewName".to_string()),
                    description: Some(Some("new desc".to_string())),
                    ..Default::default()
                },
            )
            .await
            .expect("update_entity");

        let fts = rt.text(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", updated.id)
            .await
            .expect("get_document")
            .expect("document must exist after update_entity");

        let expected = entity_fts_document(&updated);

        assert_eq!(stored.title, expected.title, "title after update");
        assert_eq!(stored.body, expected.body, "body after update");
    }

    // Verify that merge_entity / merge_note delete from_id vectors from ALL
    // registered model vec tables, not just the default-model table. Uses the
    // same ConstVecProvider/ConstVecService pattern as operations.rs so no
    // real model files are required.

    struct MergeTestVecService {
        dims: usize,
    }

    #[async_trait::async_trait]
    impl lattice_embed::EmbeddingService for MergeTestVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: lattice_embed::EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "merge-test-const-vec"
        }
    }

    struct MergeTestVecProvider {
        provider_name: String,
        dims: usize,
    }

    impl MergeTestVecProvider {
        fn new(name: &str, dims: usize) -> Self {
            Self {
                provider_name: name.to_owned(),
                dims,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::embedder_registry::EmbedderProvider for MergeTestVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(
            &self,
        ) -> crate::error::RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>>
        {
            Ok(std::sync::Arc::new(MergeTestVecService { dims: self.dims }))
        }
    }

    #[tokio::test]
    async fn entity_reindex_with_captured_merge_plan_excludes_late_model() {
        const DIMS: usize = 4;
        const PLANNED: &str = "merge-entity-plan-existing";
        const LATE: &str = "merge-entity-plan-late";
        let rt = KhiveRuntime::memory().unwrap();
        let ns = crate::Namespace::parse("merge-entity-plan-snapshot").unwrap();
        let tok = NamespaceToken::for_namespace(ns);
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "CapturedEntityPlan",
                Some("full source remains indexed"),
                None,
                vec![],
            )
            .await
            .expect("create entity before registering embedders");

        rt.register_embedder(MergeTestVecProvider::new(PLANNED, DIMS));
        let embedding_plan = EmbeddingModelPlan::capture(&rt);
        rt.register_embedder(MergeTestVecProvider::new(LATE, DIMS));

        rt.reindex_entity_with_plan(&tok, &entity, &embedding_plan)
            .await
            .expect("reindex entity with captured merge plan");

        assert_eq!(embedding_plan.model_names().len(), 1);
        assert_eq!(embedding_plan.model_names()[0].as_str(), PLANNED);
        assert_eq!(
            rt.vectors_for_model(&tok, PLANNED)
                .unwrap()
                .count()
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            rt.vectors_for_model(&tok, LATE)
                .unwrap()
                .count()
                .await
                .unwrap(),
            0,
            "a provider registered after plan capture must not join survivor reindex"
        );
    }

    #[tokio::test]
    async fn note_reindex_with_captured_merge_plan_excludes_late_model() {
        const DIMS: usize = 4;
        const PLANNED: &str = "merge-note-plan-existing";
        const LATE: &str = "merge-note-plan-late";
        let rt = KhiveRuntime::memory().unwrap();
        let ns = crate::Namespace::parse("merge-note-plan-snapshot").unwrap();
        let tok = NamespaceToken::for_namespace(ns);
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "full note source remains indexed",
                None,
                None,
                vec![],
            )
            .await
            .expect("create note before registering embedders");

        rt.register_embedder(MergeTestVecProvider::new(PLANNED, DIMS));
        let embedding_plan = EmbeddingModelPlan::capture(&rt);
        rt.register_embedder(MergeTestVecProvider::new(LATE, DIMS));

        rt.reindex_note_with_plan(&tok, &note, &embedding_plan)
            .await
            .expect("reindex note with captured merge plan");

        assert_eq!(embedding_plan.model_names().len(), 1);
        assert_eq!(embedding_plan.model_names()[0].as_str(), PLANNED);
        assert_eq!(
            rt.vectors_for_model(&tok, PLANNED)
                .unwrap()
                .count()
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            rt.vectors_for_model(&tok, LATE)
                .unwrap()
                .count()
                .await
                .unwrap(),
            0,
            "a provider registered after plan capture must not join survivor reindex"
        );
    }

    /// merge_entity must delete from_id vectors from ALL registered model tables.
    ///
    /// Two custom embedders ("merge-vec-a", "merge-vec-b") are registered.  Both
    /// entities are embedded so each has a row in both model tables.  After merge,
    /// from_id must have zero surviving rows in either table.
    #[tokio::test]
    async fn merge_entity_clears_vectors_from_all_registered_models() {
        const DIMS: usize = 4;
        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(MergeTestVecProvider::new("merge-vec-a", DIMS));
        rt.register_embedder(MergeTestVecProvider::new("merge-vec-b", DIMS));

        let ns_str = "merge-entity-vec-cleanup";
        let ns = crate::Namespace::parse(ns_str).unwrap();
        let tok = NamespaceToken::for_namespace(ns);

        let into_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "IntoVecEntity",
                Some("desc a"),
                None,
                vec![],
            )
            .await
            .expect("create into");
        let from_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "FromVecEntity",
                Some("desc b"),
                None,
                vec![],
            )
            .await
            .expect("create from");

        // Confirm both entities have vectors in both model tables before merge.
        let vs_a = rt.vectors_for_model(&tok, "merge-vec-a").unwrap();
        let vs_b = rt.vectors_for_model(&tok, "merge-vec-b").unwrap();
        use khive_storage::types::VectorSearchRequest;
        let query = vec![1.0_f32; DIMS];
        let pre_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_a.iter().any(|h| h.subject_id == into_e.id)
                && pre_a.iter().any(|h| h.subject_id == from_e.id),
            "both entities must be in model-a before merge; got {pre_a:?}"
        );

        // model-b must ALSO hold both entities pre-merge, else the post-merge
        // model-b emptiness check below is vacuous (nothing to delete).
        let pre_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_b.iter().any(|h| h.subject_id == into_e.id)
                && pre_b.iter().any(|h| h.subject_id == from_e.id),
            "both entities must be in model-b before merge; got {pre_b:?}"
        );

        rt.merge_entity_with_reason(
            &tok,
            into_e.id,
            from_e.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
            None,
        )
        .await
        .expect("merge_entity");

        let post_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_a: Vec<_> = post_a
            .iter()
            .filter(|h| h.subject_id == from_e.id)
            .collect();
        assert!(
            from_ids_a.is_empty(),
            "from_id must have no vectors in model-a after merge; got {from_ids_a:?}"
        );

        let post_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("merge-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_b: Vec<_> = post_b
            .iter()
            .filter(|h| h.subject_id == from_e.id)
            .collect();
        assert!(
            from_ids_b.is_empty(),
            "from_id must have no vectors in model-b after merge; got {from_ids_b:?}"
        );
    }

    /// merge_note must delete from_id vectors from ALL registered model tables.
    ///
    /// Two custom embedders ("merge-note-vec-a", "merge-note-vec-b") are registered.
    /// Both notes are embedded so each has a row in both model tables.  After merge,
    /// from_id must have zero surviving rows in either table.
    #[tokio::test]
    async fn merge_note_clears_vectors_from_all_registered_models() {
        const DIMS: usize = 4;
        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(MergeTestVecProvider::new("merge-note-vec-a", DIMS));
        rt.register_embedder(MergeTestVecProvider::new("merge-note-vec-b", DIMS));

        let ns_str = "merge-note-vec-cleanup";
        let ns = crate::Namespace::parse(ns_str).unwrap();
        let tok = NamespaceToken::for_namespace(ns);

        let into_n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "IntoVecNote content",
                None,
                None,
                vec![],
            )
            .await
            .expect("create into note");
        let from_n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "FromVecNote content",
                None,
                None,
                vec![],
            )
            .await
            .expect("create from note");

        let vs_a = rt.vectors_for_model(&tok, "merge-note-vec-a").unwrap();
        let vs_b = rt.vectors_for_model(&tok, "merge-note-vec-b").unwrap();
        use khive_storage::types::VectorSearchRequest;
        let query = vec![1.0_f32; DIMS];

        let pre_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_a.iter().any(|h| h.subject_id == into_n.id)
                && pre_a.iter().any(|h| h.subject_id == from_n.id),
            "both notes must be in model-a before merge; got {pre_a:?}"
        );

        // model-b must ALSO hold both notes pre-merge, else the post-merge
        // model-b emptiness check below is vacuous (nothing to delete).
        let pre_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            pre_b.iter().any(|h| h.subject_id == into_n.id)
                && pre_b.iter().any(|h| h.subject_id == from_n.id),
            "both notes must be in model-b before merge; got {pre_b:?}"
        );

        rt.merge_note_with_reason(
            &tok,
            into_n.id,
            from_n.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::PreferInto,
            false,
            None,
        )
        .await
        .expect("merge_note");

        let post_a = vs_a
            .search(VectorSearchRequest {
                query_vectors: vec![query.clone()],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_a: Vec<_> = post_a
            .iter()
            .filter(|h| h.subject_id == from_n.id)
            .collect();
        assert!(
            from_ids_a.is_empty(),
            "from_id must have no vectors in model-a after merge; got {from_ids_a:?}"
        );

        let post_b = vs_b
            .search(VectorSearchRequest {
                query_vectors: vec![query],
                top_k: 100,
                namespace: Some(ns_str.to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("merge-note-vec-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        let from_ids_b: Vec<_> = post_b
            .iter()
            .filter(|h| h.subject_id == from_n.id)
            .collect();
        assert!(
            from_ids_b.is_empty(),
            "from_id must have no vectors in model-b after merge; got {from_ids_b:?}"
        );
    }

    // Cross-path equality: merge_entity must produce a stored FTS document for
    // the kept entity that is field-identical to entity_fts_document().
    #[tokio::test]
    async fn entity_fts_document_matches_runtime_merge_path() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let into_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "IntoEntity",
                Some("into desc"),
                None,
                vec![],
            )
            .await
            .expect("create into");
        let from_e = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "FromEntity",
                Some("from desc"),
                None,
                vec![],
            )
            .await
            .expect("create from");

        let summary = rt
            .merge_entity_with_reason(
                &tok,
                into_e.id,
                from_e.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
                None,
            )
            .await
            .expect("merge_entity");

        let kept = rt
            .get_entity(&tok, summary.kept_id)
            .await
            .expect("get kept");

        let fts = rt.text(&tok).expect("FTS store");
        let stored = fts
            .get_document("local", kept.id)
            .await
            .expect("get_document")
            .expect("FTS document must exist for kept entity after merge");

        let expected = entity_fts_document(&kept);

        assert_eq!(stored.title, expected.title, "title after merge");
        assert_eq!(stored.body, expected.body, "body after merge");
    }

    /// The recomputed `properties_merged` count must agree with the fold on
    /// whole-value replacement.
    ///
    /// `merge_json` scores a `PreferFrom` replace of one properties value by a
    /// differently-shaped one as a single contribution. An earlier version of
    /// `count_new_property_keys` returned 0 for that shape, which under-reported
    /// every such merge — including on note kinds with no owner-established
    /// properties, which never enter the restoration path and were being counted
    /// correctly before the recompute was introduced. Measured at the time:
    /// fold=1, recompute=0, on both orderings.
    ///
    /// The flat-overwrite control is load-bearing: it is what distinguishes this
    /// test from one that a function returning 1 unconditionally would also pass.
    #[test]
    fn recomputed_count_agrees_with_fold_on_whole_value_replacement() {
        use serde_json::json;

        for (into, from, label) in [
            (json!({"a": 1}), json!(5), "object replaced by scalar"),
            (
                json!(7),
                json!({"b": 2}),
                "scalar replaced by single-key object",
            ),
            // A whole-value replacement is ONE contribution however many keys
            // the replacing object carries. The single-key vector above cannot
            // see the difference between that rule and counting the object's
            // keys, so it stayed green while the count was wrong for ordinary
            // notes. This vector is the one that distinguishes them.
            (
                json!(7),
                json!({"b": 2, "c": 3}),
                "scalar replaced by multi-key object",
            ),
        ] {
            let (merged, fold_count) = merge_json(&into, &from, EntityDedupMergePolicy::PreferFrom);
            let recomputed = count_new_property_keys(
                Some(&into),
                Some(&merged),
                EntityDedupMergePolicy::PreferFrom,
            );
            assert_eq!(
                recomputed, fold_count,
                "{label}: recomputed count must match the fold's own count",
            );
            assert_eq!(recomputed, 1, "{label}: one value was contributed");
        }

        // Control: an ordinary overwrite of an existing key contributes nothing
        // under BOTH rules. Without this arm, a function returning 1 for every
        // differing pair would pass the loop above.
        let (merged, fold_count) = merge_json(
            &json!({"a": 1}),
            &json!({"a": 2}),
            EntityDedupMergePolicy::PreferFrom,
        );
        let recomputed = count_new_property_keys(
            Some(&json!({"a": 1})),
            Some(&merged),
            EntityDedupMergePolicy::PreferFrom,
        );
        assert_eq!(fold_count, 0, "control: overwrite is never a fold addition");
        assert_eq!(recomputed, 0, "control: overwrite is never a new key");

        // Control: equal values mean nothing was contributed (the `from` note
        // carrying no properties at all).
        assert_eq!(
            count_new_property_keys(
                Some(&json!({"a": 1})),
                Some(&json!({"a": 1})),
                EntityDedupMergePolicy::PreferFrom,
            ),
            0,
            "control: an unchanged properties object contributes nothing",
        );
    }

    /// Recursion into a same-named nested object is only correct under `Union`.
    ///
    /// `merge_json` descends into a nested object ONLY for `Union`. Under
    /// `PreferFrom` an existing top-level key is replaced wholesale and under
    /// `PreferInto` it is kept wholesale, so nothing is merged beneath that key
    /// and nothing beneath it may be counted. An earlier version of the
    /// recomputation recursed unconditionally and reported 1 for the
    /// `PreferFrom` case below, where one existing property was replaced and
    /// none was added. This affects ordinary notes with no owner-established
    /// properties, which never reach the restoration path at all.
    #[test]
    fn recomputed_count_recurses_into_nested_objects_only_under_union() {
        use serde_json::json;

        let into = json!({"meta": {"old": 1}});
        let from = json!({"meta": {"new": 2}});

        for (strategy, expected, label) in [
            (
                EntityDedupMergePolicy::PreferFrom,
                0,
                "prefer_from replaces the whole key",
            ),
            (
                EntityDedupMergePolicy::PreferInto,
                0,
                "prefer_into keeps the whole key",
            ),
            (
                EntityDedupMergePolicy::Union,
                1,
                "union merges beneath the key",
            ),
        ] {
            let (merged, fold_count) = merge_json(&into, &from, strategy);
            let recomputed = count_new_property_keys(Some(&into), Some(&merged), strategy);
            assert_eq!(
                recomputed, fold_count,
                "{label}: recomputed count must match the fold's own count",
            );
            assert_eq!(recomputed, expected, "{label}");
        }
    }

    /// An object emptied by restoration contributed nothing, and the count must
    /// say so.
    ///
    /// When the surviving record's properties were not an object and the fold
    /// installed the from-note's object, restoration removes the owner keys the
    /// survivor never had — which can leave `{}`. Scoring that as a whole-value
    /// replacement would report 1 for a record holding no properties at all.
    #[test]
    fn recomputed_count_is_zero_when_restoration_empties_the_object() {
        use serde_json::json;

        assert_eq!(
            count_new_property_keys(
                Some(&json!("scalar-properties")),
                Some(&json!({})),
                EntityDedupMergePolicy::PreferFrom,
            ),
            0,
            "an emptied object retains nothing from the absorbed record",
        );

        // Control: the same shape with a surviving key counts that key, so the
        // arm above is not simply returning 0 for every non-object original.
        assert_eq!(
            count_new_property_keys(
                Some(&json!("scalar-properties")),
                Some(&json!({"kept": 1})),
                EntityDedupMergePolicy::PreferFrom,
            ),
            1,
            "a surviving key is still counted",
        );
    }

    // ---- merge transaction budget tests ----

    /// Run `merge_entity_sql` directly on the writer connection with explicit
    /// limits, mapping the two-variant error the way the production fallback
    /// path does. The budget refusal must surface as the SQLite-side error
    /// whose message carries the observed counts.
    async fn run_entity_merge_with_limits(
        rt: &KhiveRuntime,
        into_id: Uuid,
        from_id: Uuid,
        limits: MergeTxLimits,
    ) -> Result<(MergeSummary, Entity), SqliteError> {
        let pack_rules = rt.pack_edge_rules();
        let pool = rt.backend().pool_arc();
        tokio::task::spawn_blocking(move || {
            let guard = pool.writer().unwrap();
            guard.transaction(|conn| {
                merge_entity_sql(
                    conn,
                    "local".to_string(),
                    "fts_entities".to_string(),
                    Vec::new(),
                    into_id,
                    from_id,
                    EntityDedupMergePolicy::PreferInto,
                    ContentMergeStrategy::Append,
                    false,
                    pack_rules,
                    EntityMergeValidation::LegacyKind,
                    limits,
                )
                .map_err(|error| match error {
                    MergeEntitySqlError::Sqlite(error) => error,
                    MergeEntitySqlError::Refusal(_) => SqliteError::InvalidData(
                        "unexpected transactional policy refusal".to_string(),
                    ),
                })
            })
        })
        .await
        .unwrap()
    }

    /// Preview-only variant of [`run_entity_merge_with_limits`]: runs
    /// `merge_entity_sql` with `dry_run = true` and an unlimited budget, and
    /// returns the observed byte charge without committing any write. Lets a
    /// test read back the probe's true cost for a record and then reuse that
    /// exact number to place a tight `MergeTxLimits` threshold, instead of
    /// guessing at fanout/overhead constants.
    async fn preview_entity_merge_bytes(rt: &KhiveRuntime, into_id: Uuid, from_id: Uuid) -> usize {
        let pack_rules = rt.pack_edge_rules();
        let pool = rt.backend().pool_arc();
        let (summary, _) = tokio::task::spawn_blocking(move || {
            let guard = pool.writer().unwrap();
            guard.transaction(|conn| {
                merge_entity_sql(
                    conn,
                    "local".to_string(),
                    "fts_entities".to_string(),
                    Vec::new(),
                    into_id,
                    from_id,
                    EntityDedupMergePolicy::PreferInto,
                    ContentMergeStrategy::Append,
                    true,
                    pack_rules,
                    EntityMergeValidation::LegacyKind,
                    MergeTxLimits {
                        max_rows: usize::MAX,
                        max_bytes: usize::MAX,
                    },
                )
                .map_err(|error| match error {
                    MergeEntitySqlError::Sqlite(error) => error,
                    MergeEntitySqlError::Refusal(_) => SqliteError::InvalidData(
                        "unexpected transactional policy refusal".to_string(),
                    ),
                })
            })
        })
        .await
        .unwrap()
        .unwrap();
        summary.tx_budget.bytes_charged
    }

    /// The byte-budget probe must count actual UTF-8 bytes, not SQLite's
    /// `LENGTH(text)` character count. Two records with an identical
    /// character count but different UTF-8 byte sizes (an ASCII control vs.
    /// a CJK payload, each 200 characters) must charge the budget
    /// differently — proving the probe casts to BLOB before measuring —
    /// and a budget threshold placed strictly between the two true costs
    /// must accept the ASCII control and reject the multibyte payload.
    #[tokio::test]
    async fn merge_entity_byte_budget_rejects_multibyte_properties_char_count_would_pass() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let ascii_payload = "x".repeat(200);
        let multibyte_payload = "\u{4e2d}".repeat(200);
        assert_eq!(
            ascii_payload.chars().count(),
            multibyte_payload.chars().count(),
            "control and payload must share one character count"
        );
        assert!(
            multibyte_payload.len() > ascii_payload.len(),
            "multibyte payload must have more UTF-8 bytes than the ASCII control"
        );

        let into_ascii = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from_ascii = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({ "note": ascii_payload })),
                vec![],
            )
            .await
            .unwrap();
        let into_multi = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from_multi = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "From",
                None,
                Some(serde_json::json!({ "note": multibyte_payload })),
                vec![],
            )
            .await
            .unwrap();

        let ascii_total_bytes = preview_entity_merge_bytes(&rt, into_ascii.id, from_ascii.id).await;
        let multi_total_bytes = preview_entity_merge_bytes(&rt, into_multi.id, from_multi.id).await;
        assert!(
            multi_total_bytes > ascii_total_bytes,
            "byte-accurate probe must charge more for the multibyte record: \
             ascii={ascii_total_bytes} multi={multi_total_bytes}"
        );

        // A threshold pinned exactly at the ASCII control's true cost must
        // accept it and reject the multibyte record, which a character-
        // counting probe would have under-charged into passing too.
        let limits = MergeTxLimits {
            max_rows: usize::MAX,
            max_bytes: ascii_total_bytes,
        };

        run_entity_merge_with_limits(&rt, into_ascii.id, from_ascii.id, limits)
            .await
            .expect("ASCII control's true byte cost must fit its own threshold");

        let error = run_entity_merge_with_limits(&rt, into_multi.id, from_multi.id, limits)
            .await
            .unwrap_err();
        let msg = error.to_string();
        assert!(
            msg.contains("merge transaction budget exceeded"),
            "multibyte properties must be rejected by the byte-accurate probe; got: {msg}"
        );
        assert!(msg.contains("reading merge records"), "got: {msg}");
        assert!(
            rt.get_entity(&tok, from_multi.id).await.is_ok(),
            "from-entity must survive a budget-rejected merge"
        );
    }

    async fn run_note_merge_with_limits(
        rt: &KhiveRuntime,
        into_id: Uuid,
        from_id: Uuid,
        pack_rules: Vec<khive_types::EdgeEndpointRule>,
        limits: MergeTxLimits,
    ) -> Result<(MergeSummary, Note), SqliteError> {
        let pool = rt.backend().pool_arc();
        tokio::task::spawn_blocking(move || {
            let guard = pool.writer().unwrap();
            guard.transaction(|conn| {
                merge_note_sql(
                    conn,
                    "local".to_string(),
                    "fts_notes".to_string(),
                    Vec::new(),
                    into_id,
                    from_id,
                    EntityDedupMergePolicy::PreferInto,
                    ContentMergeStrategy::Append,
                    false,
                    pack_rules,
                    false,
                    limits,
                )
            })
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn merge_entity_rejects_row_budget_while_collecting_incident_edges() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        for name in ["T1", "T2", "T3"] {
            let target = rt
                .create_entity(&tok, "concept", None, name, None, None, vec![])
                .await
                .unwrap();
            rt.link(&tok, from.id, target.id, EdgeRelation::Extends, 1.0, None)
                .await
                .unwrap();
        }

        // Two merge records charge first; the cap of 4 admits the first two
        // incident edges and trips on the third, before it is retained.
        let error = run_entity_merge_with_limits(
            &rt,
            into.id,
            from.id,
            MergeTxLimits {
                max_rows: 4,
                max_bytes: usize::MAX,
            },
        )
        .await
        .unwrap_err();
        let msg = error.to_string();
        assert!(
            msg.contains("merge transaction budget exceeded"),
            "got: {msg}"
        );
        assert!(msg.contains("collecting incident edges"), "got: {msg}");

        // The rejected transaction must roll back completely.
        assert!(
            rt.get_entity(&tok, from.id).await.is_ok(),
            "from-entity must survive a budget-rejected merge"
        );
        let edges = rt
            .list_edges(
                &tok,
                EdgeListFilter {
                    source_id: Some(from.id),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            edges.len(),
            3,
            "every incident edge must survive a budget-rejected merge"
        );
    }

    #[tokio::test]
    async fn merge_entity_rejects_row_budget_while_collecting_conflict_cascade_rows() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let shared = rt
            .create_entity(&tok, "concept", None, "Shared", None, None, vec![])
            .await
            .unwrap();
        let annotator = rt
            .create_note(
                &tok,
                "observation",
                None,
                "annotator note",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let nested_annotator = rt
            .create_note(&tok, "observation", None, "nested note", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, into.id, shared.id, EdgeRelation::Extends, 0.9, None)
            .await
            .unwrap();
        let dropped = rt
            .link(&tok, from.id, shared.id, EdgeRelation::Extends, 0.2, None)
            .await
            .unwrap();
        let annotation = rt
            .link(
                &tok,
                annotator.id,
                dropped.id.into(),
                EdgeRelation::Annotates,
                0.7,
                None,
            )
            .await
            .unwrap();
        let nested_annotation = rt
            .link(
                &tok,
                nested_annotator.id,
                annotation.id.into(),
                EdgeRelation::Annotates,
                0.6,
                None,
            )
            .await
            .unwrap();

        // Row walk under a cap of 5: two merge records, one incident edge,
        // one endpoint-contract resolution, then the natural-key conflict's
        // recursive cascade collection charges the annotation chain and trips
        // on its second (nested) row.
        let error = run_entity_merge_with_limits(
            &rt,
            into.id,
            from.id,
            MergeTxLimits {
                max_rows: 5,
                max_bytes: usize::MAX,
            },
        )
        .await
        .unwrap_err();
        let msg = error.to_string();
        assert!(
            msg.contains("merge transaction budget exceeded"),
            "got: {msg}"
        );
        assert!(
            msg.contains("collecting conflict cascade rows"),
            "got: {msg}"
        );

        // Roll back means the whole annotation chain is still present.
        for id in [dropped.id, annotation.id, nested_annotation.id] {
            assert!(
                rt.get_edge_including_deleted(&tok, id.into())
                    .await
                    .unwrap()
                    .is_some(),
                "edge {id} must survive a budget-rejected merge"
            );
        }
        assert!(
            rt.get_entity(&tok, from.id).await.is_ok(),
            "from-entity must survive a budget-rejected merge"
        );
    }

    #[tokio::test]
    async fn merge_note_rejects_byte_budget_while_reading_merge_records() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(
                &tok,
                "observation",
                None,
                "into content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let fat = "x".repeat(8192);
        let from = rt
            .create_note(&tok, "observation", None, &fat, None, None, vec![])
            .await
            .unwrap();

        let error = run_note_merge_with_limits(
            &rt,
            into.id,
            from.id,
            Vec::new(),
            MergeTxLimits {
                max_rows: usize::MAX,
                max_bytes: 4096,
            },
        )
        .await
        .unwrap_err();
        let msg = error.to_string();
        assert!(
            msg.contains("merge transaction budget exceeded"),
            "got: {msg}"
        );
        assert!(msg.contains("reading merge records"), "got: {msg}");

        assert!(
            rt.notes(&tok)
                .unwrap()
                .get_note(from.id)
                .await
                .unwrap()
                .is_some(),
            "from-note must survive a budget-rejected merge"
        );
    }

    /// The byte budget must be charged from a cheap SQL-side length probe
    /// BEFORE the merge fully loads and JSON-parses a record's `properties`
    /// column — never after. Prove it adversarially: store an oversized
    /// `properties` value that is also invalid JSON directly on `from`,
    /// bypassing the create path's own validation. If the budget were still
    /// charged only after `read_merge_entity`'s full load-and-parse (the
    /// pre-fix ordering), this merge would fail with a JSON parse error
    /// instead of a budget error, because the parse would run before the
    /// stale post-read charge was ever reached. Charging from the pre-parse
    /// length probe must reject on budget first, so `serde_json::from_str`
    /// never runs on this column at all.
    #[tokio::test]
    async fn merge_entity_rejects_byte_budget_before_parsing_oversized_malformed_properties() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();

        let huge_malformed_properties = format!("{{not valid json: {}", "x".repeat(8192));
        let pool = rt.backend().pool_arc();
        let from_id = from.id;
        tokio::task::spawn_blocking(move || {
            let guard = pool.writer().unwrap();
            guard.transaction(|conn| {
                conn.execute(
                    "UPDATE entities SET properties = ?1 WHERE id = ?2",
                    rusqlite::params![huge_malformed_properties, from_id.to_string()],
                )?;
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();

        let error = run_entity_merge_with_limits(
            &rt,
            into.id,
            from.id,
            MergeTxLimits {
                max_rows: usize::MAX,
                max_bytes: 4096,
            },
        )
        .await
        .unwrap_err();
        let msg = error.to_string();
        assert!(
            msg.contains("merge transaction budget exceeded"),
            "expected an early budget rejection, not a JSON parse failure; got: {msg}"
        );
        assert!(msg.contains("reading merge records"), "got: {msg}");

        // `get_entity` would itself fail to parse the malformed properties this
        // test deliberately stored, so check survival via a raw row count
        // instead of the parsing read path.
        let pool = rt.backend().pool_arc();
        let still_present: i64 = tokio::task::spawn_blocking(move || {
            let guard = pool.writer().unwrap();
            guard.transaction(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM entities WHERE id = ?1 AND deleted_at IS NULL",
                    rusqlite::params![from_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(SqliteError::Rusqlite)
            })
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            still_present, 1,
            "from-entity must survive a budget-rejected merge"
        );
    }

    #[tokio::test]
    async fn merge_note_rejects_row_budget_while_collecting_incident_edges() {
        use khive_storage::EdgeRelation;
        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();
        for name in ["T1", "T2", "T3"] {
            let target = rt
                .create_entity(&tok, "concept", None, name, None, None, vec![])
                .await
                .unwrap();
            rt.link(&tok, from.id, target.id, EdgeRelation::Annotates, 1.0, None)
                .await
                .unwrap();
        }

        let error = run_note_merge_with_limits(
            &rt,
            into.id,
            from.id,
            rt.pack_edge_rules(),
            MergeTxLimits {
                max_rows: 4,
                max_bytes: usize::MAX,
            },
        )
        .await
        .unwrap_err();
        let msg = error.to_string();
        assert!(
            msg.contains("merge transaction budget exceeded"),
            "got: {msg}"
        );
        assert!(msg.contains("collecting incident edges"), "got: {msg}");

        let edges = rt
            .list_edges(
                &tok,
                EdgeListFilter {
                    source_id: Some(from.id),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(
            edges.len(),
            3,
            "every incident edge must survive a budget-rejected merge"
        );
    }

    // The post-commit budget logs are captured by the process-global tracing
    // subscriber owned by `crate::pack::tests` — one test binary supports at
    // most one `set_global_default`, and a thread-local `set_default` guard
    // here proved lossy under parallel tests (the same event-loss class the
    // pack tests' subscriber documents). Each test selects its own rows from
    // the append-only sink by the merge's `into_id`.
    use crate::pack::tests::budget_log_events;

    #[tokio::test]
    async fn merge_entity_reports_and_logs_tx_budget_after_commit() {
        use khive_storage::EdgeRelation;
        let events = budget_log_events();

        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_entity(&tok, "concept", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_entity(&tok, "concept", None, "From", None, None, vec![])
            .await
            .unwrap();
        let target = rt
            .create_entity(&tok, "concept", None, "Target", None, None, vec![])
            .await
            .unwrap();
        rt.link(&tok, from.id, target.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // A dry run reports the same predictive budget usage but must not
        // emit the post-commit log: nothing committed.
        let preview = rt
            .merge_entity(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                true,
            )
            .await
            .unwrap();
        assert!(preview.tx_budget.rows_charged >= 2);
        assert_eq!(preview.tx_budget.max_rows, MERGE_TX_MAX_ROWS);
        assert_eq!(preview.tx_budget.max_bytes, MERGE_TX_MAX_BYTES);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|e| e.into_id != into.id.to_string()),
            "a dry-run preview must not emit the post-commit budget log"
        );

        let summary = rt
            .merge_entity(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .unwrap();
        assert!(summary.tx_budget.rows_charged >= 2);
        assert!(summary.tx_budget.bytes_charged > 0);

        let captured = events.lock().unwrap();
        let row = captured
            .iter()
            .find(|e| {
                e.into_id == summary.kept_id.to_string()
                    && e.message == "merge_entity: transaction materialization budget"
            })
            .expect("committing entity merge must emit the post-commit budget log");
        assert_eq!(
            row.budget_rows as usize, summary.tx_budget.rows_charged,
            "the log must carry the same observed row count the summary reports"
        );
    }

    #[tokio::test]
    async fn merge_note_reports_and_logs_tx_budget_after_commit() {
        let events = budget_log_events();

        let rt = rt();
        let tok = NamespaceToken::local();
        let into = rt
            .create_note(&tok, "observation", None, "Into", None, None, vec![])
            .await
            .unwrap();
        let from = rt
            .create_note(&tok, "observation", None, "From", None, None, vec![])
            .await
            .unwrap();

        let summary = rt
            .merge_note(
                &tok,
                into.id,
                from.id,
                EntityDedupMergePolicy::PreferInto,
                ContentMergeStrategy::Append,
                false,
            )
            .await
            .unwrap();
        assert!(summary.tx_budget.rows_charged >= 2);
        assert!(summary.tx_budget.bytes_charged > 0);

        let captured = events.lock().unwrap();
        let row = captured
            .iter()
            .find(|e| {
                e.into_id == summary.kept_id.to_string()
                    && e.message == "merge_note: transaction materialization budget"
            })
            .expect("committing note merge must emit the post-commit budget log");
        assert_eq!(
            row.budget_rows as usize, summary.tx_budget.rows_charged,
            "the log must carry the same observed row count the summary reports"
        );
    }

    // ── Universal reserved-key reservation (ADR-115 Amendment 1, first rung) ──

    fn reserved_key_props() -> serde_json::Value {
        serde_json::json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"})
    }

    #[tokio::test]
    async fn update_entity_rejects_reserved_secret_gate_key() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "reservation-target-entity",
                None,
                Some(serde_json::json!({"k": "v"})),
                vec![],
            )
            .await
            .unwrap();

        let err = rt
            .update_entity(
                &tok,
                entity.id,
                EntityPatch {
                    properties: Some(reserved_key_props()),
                    ..Default::default()
                },
            )
            .await
            .expect_err("caller-supplied reserved key must be rejected on patch update");
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
            "unexpected error: {err:?}"
        );

        // No partial mutation: the original properties must be unchanged.
        let unchanged = rt
            .entities(&tok)
            .unwrap()
            .get_entity(entity.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.properties, Some(serde_json::json!({"k": "v"})));
    }

    #[tokio::test]
    async fn update_note_rejects_reserved_secret_gate_key() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "reservation target note",
                None,
                Some(serde_json::json!({"k": "v"})),
                vec![],
            )
            .await
            .unwrap();

        let err = rt
            .update_note(
                &tok,
                note.id,
                NotePatch::new(None, None, None, None, Some(reserved_key_props())),
            )
            .await
            .expect_err("caller-supplied reserved key must be rejected on patch update");
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
            "unexpected error: {err:?}"
        );

        let unchanged = rt
            .notes(&tok)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.properties, Some(serde_json::json!({"k": "v"})));
    }
}
