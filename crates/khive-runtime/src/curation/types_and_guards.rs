use super::{
    Deserialize, Details, EdgeFilter, EdgeRelation, EdgeRow, Entity, EventAttribution, EventKind,
    HashSet, KhiveError, KhiveRuntime, RuntimeError, RuntimeResult, Serialize, SqliteError,
    SubstrateKind, Uuid, Value,
};

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
pub(super) struct EmbeddingModelPlan {
    pub(super) model_names: Vec<String>,
}

impl EmbeddingModelPlan {
    pub(super) fn capture(runtime: &KhiveRuntime) -> Self {
        Self {
            model_names: runtime.registered_embedding_model_names(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.model_names.is_empty()
    }

    pub(super) fn model_names(&self) -> &[String] {
        &self.model_names
    }

    pub(super) fn vector_tables(&self) -> Vec<String> {
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

    /// What this guard compared, phrased for the caller that hit it.
    ///
    /// The refusal a caller reads has to say what was looked at, because the
    /// guard name alone ("name_similarity") does not tell a caller which two
    /// fields it has to change to make the merge acceptable.
    pub fn compared(self) -> &'static str {
        match self {
            Self::EntityKind => "the records' entity kinds",
            Self::NameSimilarity => "the records' names",
            Self::ProjectCompatibility => "the project lists in the records' properties",
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

/// The sentence a safety-floor refusal shows its caller.
///
/// It names the check, says what that check compared, and gives the caller an
/// action it can actually take. It deliberately does not name the override
/// parameter: the override exists for a developer integrating this runtime, and
/// a consumer principal that is handed the parameter's name spends its next turn
/// retrying with it instead of looking at the two records.
pub fn entity_merge_guard_refusal_message(guard: EntityMergeGuard) -> String {
    format!(
        "entity merge refused by the {} check, which compared {}; make the records agree on \
         that check before merging, or ask an operator to authorize an override",
        guard.as_str(),
        guard.compared()
    )
}

/// The two values `guard` compared, for a caller-facing preview of the refusal.
///
/// The project side returns whatever that property holds, or `null` when the
/// record has none. `projects_are_disjoint` refuses only on two non-empty
/// arrays, so what a caller is shown here is what the guard read.
pub fn entity_merge_guard_compared_values(
    guard: EntityMergeGuard,
    into: &Entity,
    from: &Entity,
) -> (Value, Value) {
    let projects = |entity: &Entity| {
        entity
            .properties
            .as_ref()
            .and_then(|properties| properties.get("projects"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    match guard {
        EntityMergeGuard::EntityKind => (
            Value::String(into.kind.clone()),
            Value::String(from.kind.clone()),
        ),
        EntityMergeGuard::NameSimilarity => (
            Value::String(into.name.clone()),
            Value::String(from.name.clone()),
        ),
        EntityMergeGuard::ProjectCompatibility => (projects(into), projects(from)),
    }
}

/// Convert a safety-floor refusal into the merge verb's structured conflict contract.
pub fn entity_merge_guard_error(guard: EntityMergeGuard) -> RuntimeError {
    RuntimeError::Khive(
        KhiveError::conflict(entity_merge_guard_refusal_message(guard)).with_details(Details::new(
            [("guard", guard.as_str()), ("compared", guard.compared())],
        )),
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
    /// Edges dropped because both rewired endpoints resolved to the
    /// surviving record — the edge described a relationship *between* the
    /// two merge operands (e.g. `supports`/`refutes`), and once merged that
    /// relationship has no referent to point at, so it is deleted rather
    /// than kept as a self-referencing row. Distinct from
    /// `edges_contract_skipped` (an endpoint-contract rejection) and
    /// `edge_conflict_preimages` (a natural-key collision with an unrelated
    /// existing edge) — a self-loop has no such competitor.
    #[serde(default)]
    pub edges_self_loop_dropped: usize,
    /// Full preimages for the edges counted in `edges_self_loop_dropped`, in
    /// the same [`MergeEdgePreimage`] shape `edge_conflict_preimages` uses,
    /// so a dropped relationship between the merge operands is recoverable
    /// rather than silently destroyed by the row delete.
    #[serde(default)]
    pub self_loop_edge_preimages: Vec<MergeEdgePreimage>,
    /// Recursive edge annotations removed with self-loop edges. The root
    /// preimages remain in `self_loop_edge_preimages`.
    #[serde(default)]
    pub self_loop_incident_edge_preimages: Vec<MergeEdgePreimage>,
    /// Incident edges dropped instead of rewired because the rewired
    /// `(source, relation, target)` triple would violate the pack endpoint
    /// contract `link` enforces (khive#1216) — consistent with the existing
    /// dangling-endpoint skip behavior, never silently rewired into a
    /// contract-violating edge.
    #[serde(default)]
    pub edges_contract_skipped: usize,
    /// Full preimages for the contract-violating root edges counted above.
    #[serde(default)]
    pub contract_drop_edge_preimages: Vec<MergeEdgePreimage>,
    /// Recursive edge annotations removed with contract-violating edges.
    #[serde(default)]
    pub contract_drop_incident_edge_preimages: Vec<MergeEdgePreimage>,
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
    /// Error returned by the post-commit survivor reindex. A set value means
    /// the entity or note merge committed but the reindex did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_commit_reindex_error: Option<String>,
}

/// Complete stored state of an edge removed during a merge.
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
pub(super) const MERGE_TX_MAX_ROWS: usize = 50_000;

/// Default per-transaction aggregate byte cap across the same materialized
/// state (variable-length payloads: descriptions/content, properties, tags,
/// edge metadata, fanout table names).
pub(super) const MERGE_TX_MAX_BYTES: usize = 32 * 1024 * 1024;

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
pub(super) struct MergeTxBudget {
    limits: MergeTxLimits,
    rows: usize,
    bytes: usize,
}

impl MergeTxBudget {
    pub(super) fn new(limits: MergeTxLimits) -> Self {
        Self {
            limits,
            rows: 0,
            bytes: 0,
        }
    }

    /// Add `rows`/`bytes` to the account; reject once either limit is passed.
    /// Callers charge each unit of state *before* retaining it, so a rejected
    /// merge never materializes more than one row past the cap.
    pub(super) fn charge(
        &mut self,
        rows: usize,
        bytes: usize,
        context: &str,
    ) -> Result<(), SqliteError> {
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

    pub(super) fn report(&self) -> MergeTxBudgetReport {
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
pub(super) fn edge_row_budget_bytes(edge: &EdgeRow) -> usize {
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

/// Kind-owned property semantics carried from validated hook preparation to
/// the shared note merge. The default preserves ordinary JSON merge behavior.
#[derive(Clone, Debug, Default)]
pub struct NoteUpdatePolicy {
    pub(super) kind: Option<String>,
    pub(super) null_clearing_properties: &'static [&'static str],
}

impl NoteUpdatePolicy {
    pub(crate) fn for_kind(kind: &str, null_clearing_properties: &'static [&'static str]) -> Self {
        Self {
            kind: Some(kind.to_owned()),
            null_clearing_properties,
        }
    }
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
    pub write_options: crate::note_write::NoteWriteOptions,
    pub(crate) update_policy: NoteUpdatePolicy,
}

/// Normalize the public note tag replacement into its stored property before
/// kind hooks inspect the patch. An explicit list, including an empty one,
/// wins over properties.tags; omission and null preserve the property patch.
pub(crate) fn normalize_note_update_tags(args: &mut Value) -> RuntimeResult<()> {
    let args = args
        .as_object_mut()
        .ok_or_else(|| RuntimeError::InvalidInput("update arguments must be an object".into()))?;
    let Some(tags) = args.get("tags").filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let tags: Vec<String> = serde_json::from_value(tags.clone()).map_err(|error| {
        RuntimeError::InvalidInput(format!("tags must be an array of strings: {error}"))
    })?;
    let mut properties = match args.get("properties") {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(properties)) => properties.clone(),
        Some(_) => {
            return Err(RuntimeError::InvalidInput(
                "properties must be an object".into(),
            ));
        }
    };
    properties.insert("tags".into(), serde_json::json!(tags));
    args.insert("properties".into(), Value::Object(properties));
    // A hook may normalize this property further. Remove the alias so later
    // preparation cannot overwrite the hook's result by applying it again.
    args.remove("tags");
    Ok(())
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
            write_options: Default::default(),
            update_policy: Default::default(),
        }
    }

    pub fn with_write_options(mut self, options: crate::note_write::NoteWriteOptions) -> Self {
        self.write_options = options;
        self
    }

    /// Apply the owning kind's policy returned by validated hook preparation.
    /// This is not a caller-facing property-deletion parameter.
    pub fn with_update_policy(mut self, policy: NoteUpdatePolicy) -> Self {
        self.update_policy = policy;
        self
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
pub(super) enum EntityMergeValidation {
    LegacyKind,
    SafetyFloor,
    Forced,
}

#[derive(Debug)]
pub(super) enum EntityMergeRefusal {
    LegacyKind {
        into_id: Uuid,
        into_kind: String,
        from_id: Uuid,
        from_kind: String,
    },
    SafetyFloor(EntityMergeGuard),
}

impl EntityMergeRefusal {
    pub(super) fn into_runtime_error(self) -> RuntimeError {
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
pub(super) enum MergeSqlError {
    Sqlite(SqliteError),
    Refusal(RuntimeError),
}

impl MergeSqlError {
    pub(super) fn into_storage_error(
        self,
        capability: khive_storage::StorageCapability,
        operation: &'static str,
    ) -> khive_storage::StorageError {
        let failure = match &self {
            Self::Sqlite(SqliteError::Rusqlite(error)) => match error {
                rusqlite::Error::SqliteFailure(code, _)
                | rusqlite::Error::SqlInputError { error: code, .. } => {
                    Some(khive_storage::error::SqliteWriteFailure {
                        stage: khive_storage::error::SqliteWriteStage::Statement,
                        primary_code: code.extended_code & 0xff,
                        extended_code: code.extended_code,
                        settlement_unknown: false,
                    })
                }
                _ => None,
            },
            Self::Sqlite(error) => error.write_failure(),
            Self::Refusal(_) => None,
        };
        let error = khive_storage::StorageError::driver(capability, operation, self);
        match failure {
            Some(failure) => error.with_sqlite_write_failure(failure),
            None => error,
        }
    }
}

impl std::fmt::Display for MergeSqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => std::fmt::Display::fmt(error, f),
            Self::Refusal(_) => f.write_str("merge refused by transactional policy"),
        }
    }
}

impl std::error::Error for MergeSqlError {}

impl From<SqliteError> for MergeSqlError {
    fn from(error: SqliteError) -> Self {
        Self::Sqlite(error)
    }
}

impl From<rusqlite::Error> for MergeSqlError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(SqliteError::Rusqlite(error))
    }
}

/// Event data captured at the authorized runtime boundary before the merge
/// moves to a writer thread. The event itself is built from the transaction's
/// summary so destructive edge preimages are inserted before commit.
pub(super) struct MergeEventContext {
    pub(super) attribution: EventAttribution,
    pub(super) reason: Option<String>,
    pub(super) force: bool,
    pub(super) strategy: EntityDedupMergePolicy,
    pub(super) content_strategy: ContentMergeStrategy,
    pub(super) kind: EventKind,
    pub(super) substrate: SubstrateKind,
    pub(super) event_id: Option<Uuid>,
}

pub(super) fn append_merge_event_in_transaction(
    conn: &rusqlite::Connection,
    context: MergeEventContext,
    summary: &MergeSummary,
    namespace: &str,
) -> Result<(), MergeSqlError> {
    let policy = match context.strategy {
        EntityDedupMergePolicy::PreferInto => "prefer_into",
        EntityDedupMergePolicy::PreferFrom => "prefer_from",
        EntityDedupMergePolicy::Union => "union",
    };
    let mut payload = serde_json::json!({
        "into_id": summary.kept_id,
        "from_id": summary.removed_id,
        "policy": policy,
        "content_strategy": format!("{:?}", context.content_strategy),
        "edges_rewired": summary.edges_rewired,
        "edges_self_loop_dropped": summary.edges_self_loop_dropped,
        "self_loop_edge_preimages": &summary.self_loop_edge_preimages,
        "self_loop_incident_edge_preimages": &summary.self_loop_incident_edge_preimages,
        "edges_contract_skipped": summary.edges_contract_skipped,
        "contract_drop_edge_preimages": &summary.contract_drop_edge_preimages,
        "contract_drop_incident_edge_preimages": &summary.contract_drop_incident_edge_preimages,
        "edge_conflict_preimages": &summary.edge_conflict_preimages,
    });
    if let Some(reason) = context.reason {
        payload["reason"] = serde_json::Value::String(reason);
    }
    if context.force {
        payload["force"] = serde_json::Value::Bool(true);
    }
    let mut event =
        khive_storage::event::Event::new(namespace, "merge", context.kind, context.substrate, "")
            .with_target(summary.kept_id)
            .with_payload(payload);
    if let Some(event_id) = context.event_id {
        event.id = event_id;
    }
    let event = context.attribution.stamp(event);
    khive_db::stores::event::append_event_in_transaction(conn, &event)?;
    Ok(())
}

/// Recover only our semantic refusal after the writer has confirmed rollback.
/// Other sources retain the request-state envelope and every driver field.
fn recover_rolled_back_merge_refusal(
    error: khive_storage::StorageError,
) -> Result<RuntimeError, khive_storage::StorageError> {
    use khive_storage::{StorageError, WriterTaskRequestState};

    let source = match error {
        StorageError::WriterTaskRequestFailed {
            request_state: WriterTaskRequestState::TransactionRolledBack,
            source,
        } => source,
        error => return Err(error),
    };
    let source = match *source {
        StorageError::Driver {
            capability,
            operation,
            source,
        } => match source.downcast::<MergeSqlError>() {
            Ok(error) => match *error {
                MergeSqlError::Refusal(error) => return Ok(error),
                error => StorageError::driver(capability, operation, error),
            },
            Err(source) => StorageError::Driver {
                capability,
                operation,
                source,
            },
        },
        error => error,
    };
    Err(StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source: Box::new(source),
    })
}

pub(super) fn map_merge_entity_storage_error(error: khive_storage::StorageError) -> RuntimeError {
    let error = match recover_rolled_back_merge_refusal(error) {
        Ok(refusal) => return refusal,
        Err(error) => error,
    };
    match error {
        khive_storage::StorageError::Driver {
            capability,
            operation,
            source,
        } => match source.downcast::<MergeSqlError>() {
            Ok(error) => match *error {
                MergeSqlError::Sqlite(error) => RuntimeError::Sqlite(error),
                MergeSqlError::Refusal(error) => error,
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

pub(super) fn map_merge_note_storage_error(error: khive_storage::StorageError) -> RuntimeError {
    let error = match recover_rolled_back_merge_refusal(error) {
        Ok(refusal) => return refusal,
        Err(error) => error,
    };
    match error {
        khive_storage::StorageError::Driver {
            capability,
            operation,
            source,
        } => match source.downcast::<MergeSqlError>() {
            Ok(error) => match *error {
                MergeSqlError::Refusal(error) => error,
                // Preserve the existing note route's storage error envelope.
                MergeSqlError::Sqlite(error) => RuntimeError::Storage(
                    khive_storage::StorageError::driver(capability, operation, error),
                ),
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
