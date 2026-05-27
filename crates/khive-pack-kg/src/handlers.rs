//! Verb handlers for the KG pack.
//!
//! Each handler: deserialize params from Value → validate → call runtime → serialize result.

use std::collections::HashMap;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    micros_to_iso, ContentMergeStrategy, EdgeListFilter, EdgePatch, EntityDedupMergePolicy,
    EntityPatch, KhiveRuntime, LinkSpec, MergeSummary, NamespaceToken, NotePatch, QueryResult,
    RuntimeError, VerbRegistry,
};
use khive_storage::types::{
    Direction, NeighborQuery, PageRequest, TraversalOptions, TraversalRequest,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{EdgeRelation, EntityFilter, EventFilter, EventOutcome, SubstrateKind};

use khive_types::{
    EntityKind, EventKind, ProposalChangeset, ProposalCreatedPayload, ProposalDecision,
    ProposalReviewedPayload, ProposalWithdrawnPayload,
};

use crate::vocab::NoteKind;
use crate::KgPack;

// ---- Kind canonicalization (ADR-030) ----
//
// kg's vocab (EntityKind / NoteKind) provides alias normalization for kg-owned
// kinds ("paper" → "document", "obs" → "observation", etc.). Other packs
// (gtd, future) register kinds with no aliases — those are matched against the
// merged registry vocabulary literally. The hybrid resolver tries kg's enum
// first, then falls back to registry membership.

fn canonical_entity_kind(raw: &str, registry: &VerbRegistry) -> Result<String, RuntimeError> {
    if let Ok(k) = EntityKind::from_str(raw) {
        return Ok(k.name().to_string());
    }
    let normalized = raw.trim().to_ascii_lowercase();
    if registry.all_entity_kinds().contains(&normalized.as_str()) {
        return Ok(normalized);
    }
    let mut all: Vec<&'static str> = registry.all_entity_kinds();
    all.sort_unstable();
    Err(RuntimeError::InvalidInput(format!(
        "unknown entity_kind {raw:?}; valid: {}",
        all.join(" | ")
    )))
}

pub(crate) fn canonical_note_kind(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<String, RuntimeError> {
    if let Ok(k) = NoteKind::from_str(raw) {
        return Ok(k.name().to_string());
    }
    let normalized = raw.trim().to_ascii_lowercase();
    if registry.all_note_kinds().contains(&normalized.as_str()) {
        return Ok(normalized);
    }
    let mut all: Vec<&'static str> = registry.all_note_kinds();
    all.sort_unstable();
    Err(RuntimeError::InvalidInput(format!(
        "unknown note_kind {raw:?}; valid: {}",
        all.join(" | ")
    )))
}

// ---- Granular `kind` discriminator (CRUD verbs) ----
//
// The wire-level `kind` param accepts either a substrate-level name
// (`"entity"`, `"note"`, `"edge"`) or any pack-registered granular kind
// (`"concept"`, `"task"`, …). The granular form infers the substrate from the
// registry and lets the call site skip the legacy `entity_kind` /
// `note_kind` subfield.
//
// Substrate-level names are reserved; they're matched first so that a future
// pack accidentally registering `"entity"` as a kind name doesn't shadow the
// substrate-wide form.

/// Resolved shape of a `kind` discriminator string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KindSpec {
    /// `kind="entity"` — substrate-wide, no implicit kind filter.
    Entity { specific: Option<String> },
    /// `kind="note"` — substrate-wide, no implicit kind filter.
    Note { specific: Option<String> },
    /// `kind="edge"` — only valid for `list`.
    Edge,
    /// `kind="event"` — only valid for `list`; `get` resolves events by UUID.
    Event,
    /// `kind="proposal"` — queries the `proposals_open` projection table (ADR-046).
    Proposal,
}

impl KindSpec {
    pub(crate) fn substrate_label(&self) -> &'static str {
        match self {
            KindSpec::Entity { .. } => "entity",
            KindSpec::Note { .. } => "note",
            KindSpec::Edge => "edge",
            KindSpec::Event => "event",
            KindSpec::Proposal => "proposal",
        }
    }
}

/// Resolve a wire-level `kind` value into a [`KindSpec`].
///
/// Order:
/// 1. Substrate-level reserved names (`entity` / `note` / `edge`).
/// 2. kg's typed enums (alias-tolerant — `"paper"` → `"document"`).
/// 3. Pack-registered entity kinds in `registry.all_entity_kinds()`.
/// 4. Pack-registered note kinds in `registry.all_note_kinds()`.
/// 5. Unknown → `InvalidInput` listing every legal value.
pub(crate) fn resolve_kind_spec(
    raw: &str,
    registry: &VerbRegistry,
) -> Result<KindSpec, RuntimeError> {
    let normalized = raw.trim().to_ascii_lowercase();

    match normalized.as_str() {
        "entity" => return Ok(KindSpec::Entity { specific: None }),
        "note" => return Ok(KindSpec::Note { specific: None }),
        "edge" => return Ok(KindSpec::Edge),
        "event" => return Ok(KindSpec::Event),
        "proposal" => return Ok(KindSpec::Proposal),
        _ => {}
    }

    // kg-typed enums first so aliases like "paper" → "document" still work.
    if let Ok(k) = EntityKind::from_str(raw) {
        return Ok(KindSpec::Entity {
            specific: Some(k.name().to_string()),
        });
    }
    if let Ok(k) = NoteKind::from_str(raw) {
        return Ok(KindSpec::Note {
            specific: Some(k.name().to_string()),
        });
    }

    // Pack-registered granular kinds.
    if registry.all_entity_kinds().contains(&normalized.as_str()) {
        return Ok(KindSpec::Entity {
            specific: Some(normalized),
        });
    }
    if registry.all_note_kinds().contains(&normalized.as_str()) {
        return Ok(KindSpec::Note {
            specific: Some(normalized),
        });
    }

    let mut all: Vec<String> = vec![
        "entity".into(),
        "note".into(),
        "edge".into(),
        "event".into(),
        "proposal".into(),
    ];
    all.extend(registry.all_entity_kinds().iter().map(|s| (*s).to_string()));
    all.extend(registry.all_note_kinds().iter().map(|s| (*s).to_string()));
    all.sort();
    all.dedup();
    Err(RuntimeError::InvalidInput(format!(
        "unknown kind {raw:?}; valid: {}",
        all.join(" | ")
    )))
}

/// Reconcile a granular `kind` with a legacy `entity_kind`/`note_kind` subfield.
/// If both are supplied, they must canonicalize to the same value.
fn reconcile_specific(
    spec_specific: Option<String>,
    legacy_raw: Option<&str>,
    canonicalize: impl Fn(&str) -> Result<String, RuntimeError>,
    legacy_field: &str,
) -> Result<Option<String>, RuntimeError> {
    let legacy_canonical = match legacy_raw {
        Some(s) => Some(canonicalize(s)?),
        None => None,
    };
    match (spec_specific, legacy_canonical) {
        (Some(a), Some(b)) if a != b => Err(RuntimeError::InvalidInput(format!(
            "kind={a:?} contradicts {legacy_field}={b:?}; pick one"
        ))),
        (Some(a), _) => Ok(Some(a)),
        (None, b) => Ok(b),
    }
}

// ---- Param structs (serde-only, no rmcp dependency) ----

/// One edge to attach immediately after record creation (issue #489 — create_linked convenience).
///
/// After the record is created its UUID becomes the edge source. Each spec is attempted via
/// `runtime.link(source=new_id, target=target_id, relation, weight)`. Individual failures are
/// collected and returned; the record creation is NOT rolled back.
#[derive(Deserialize)]
struct EdgeSpec {
    target_id: String,
    relation: String,
    weight: Option<f64>,
}

#[derive(Deserialize)]
struct CreateParams {
    kind: String,
    entity_type: Option<String>,
    name: Option<String>,
    description: Option<String>,
    content: Option<String>,
    salience: Option<f64>,
    annotates: Option<Vec<String>>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
    // Issue #487: opt-out of post-create similarity check (e.g. bulk imports).
    skip_dedup_check: Option<bool>,
    /// Optional edges to attach immediately after creation (issue #489).
    /// Each entry creates a `link(source=<new_id>, target=target_id, relation=...)`.
    /// Edge failures are collected and returned; record creation is never rolled back.
    edges: Option<Vec<EdgeSpec>>,
}

// ue-errors C1: deny_unknown_fields on param structs that are deserialized
// directly from user input (no hook preprocessing) so typo kwargs are
// rejected at deserialization rather than silently dropped.
// CreateParams is EXCLUDED here: `prepare_create` hooks inject fields
// (namespace, entity_kind, note_kind, title, priority, …) into the params
// Value before deserialization, so unknown-field rejection must be done
// at a higher layer for that verb.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetParams {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    kind: String,
    limit: Option<u32>,
    offset: Option<u32>,
    entity_kind: Option<String>,
    entity_type: Option<String>,
    // CC-2 fix: tags filter for list(kind=entity)
    tags: Option<Vec<String>>,
    source_id: Option<String>,
    target_id: Option<String>,
    relations: Option<Vec<String>>,
    min_weight: Option<f64>,
    max_weight: Option<f64>,
    note_kind: Option<String>,
    // message-specific filters (comm pack — properties JSON column)
    thread_id: Option<String>,
    direction: Option<String>,
    from: Option<String>,
    to: Option<String>,
    read: Option<bool>,
    // event-specific filters
    verb: Option<String>,
    verbs: Option<Vec<String>>,
    outcome: Option<String>,
    actor: Option<String>,
    substrate: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    event_kind: Option<String>,
    event_kinds: Option<Vec<String>>,
    session_id: Option<String>,
    observed: Option<Vec<String>>,
    selected: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct StatsParams {}

// ue-errors C1: deny_unknown_fields rejects typos like `nonexistent_field="x"`
// at deserialization.  All cross-substrate fields (entity, note, edge paths)
// remain valid because they are declared on the struct.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateParams {
    id: String,
    /// Optional — resolved from UUID when absent (ADR-014: UUID-only ops).
    kind: Option<String>,
    name: Option<Value>,
    description: Option<Value>,
    content: Option<String>,
    #[serde(default, deserialize_with = "tri_f64")]
    salience: Option<Option<f64>>,
    #[serde(default, deserialize_with = "tri_f64")]
    decay_factor: Option<Option<f64>>,
    properties: Option<Value>,
    tags: Option<Vec<String>>,
    relation: Option<String>,
    weight: Option<f64>,
    /// ue-kg-deep C3 fix: entity_kind is immutable after creation. Accepting
    /// this field (even though we reject it) prevents silent discard — callers
    /// get an explicit error instead of a silent no-op.
    entity_kind: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteParams {
    id: String,
    /// Optional — resolved from UUID when absent (ADR-014: UUID-only ops).
    kind: Option<String>,
    hard: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeParams {
    into_id: String,
    from_id: String,
    kind: Option<String>,
    strategy: Option<String>,
    content_strategy: Option<String>,
    dry_run: Option<bool>,
    #[allow(dead_code)]
    verbose: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    kind: String,
    query: String,
    limit: Option<u32>,
    entity_kind: Option<String>,
    entity_type: Option<String>,
    note_kind: Option<String>,
    include_superseded: Option<bool>,
    properties: Option<Value>,
    /// ue-kg-deep C4 fix: minimum score floor — results below this threshold
    /// are discarded. No default applied server-side; callers pass e.g. 0.01
    /// to suppress pure-noise hits. RRF rank-1 scores ≈ 0.016, so a floor
    /// like 0.02 reliably drops near-zero noise without hiding real matches.
    min_score: Option<f64>,
}

/// One entry in a bulk-link request (F205 / ADR-038).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkLinkEntry {
    source_id: String,
    target_id: String,
    relation: String,
    weight: Option<f64>,
    metadata: Option<Value>,
    dependency_kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkParams {
    // Singleton fields (required unless `links` is provided).
    source_id: Option<String>,
    target_id: Option<String>,
    relation: Option<String>,
    weight: Option<f64>,
    /// Edge metadata (open JSON; governed keys validated by runtime).
    metadata: Option<Value>,
    /// Shortcut for `metadata.dependency_kind` on `depends_on` edges.
    dependency_kind: Option<String>,
    /// When `true`, output uses full UUIDs and ISO 8601 timestamps instead of
    /// the default 8-char short IDs and YYYY/MM/DD date format.
    verbose: Option<bool>,
    // Bulk link fields (ADR-038).
    /// Multiple edges to create in one call.
    links: Option<Vec<BulkLinkEntry>>,
    /// When `true` (default), the entire batch is atomic — any failure rolls
    /// back all writes. When `false`, errors are collected and returned as
    /// warnings while successful entries are committed individually.
    atomic: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NeighborsParams {
    /// Accepts either `id` (canonical, ADR-148 normalized) or `node_id` (legacy).
    #[serde(alias = "node_id")]
    id: String,
    direction: Option<String>,
    limit: Option<u32>,
    min_weight: Option<f64>,
    relations: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraverseParams {
    /// Accepts either `roots` (legacy) or `ids` (normalized). Each entry may
    /// be a full UUID or an 8-char prefix; resolved via `resolve_uuid_async`.
    #[serde(alias = "ids")]
    roots: Vec<String>,
    max_depth: Option<usize>,
    direction: Option<String>,
    relations: Option<Vec<String>>,
    min_weight: Option<f64>,
    limit: Option<u32>,
    include_roots: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryParams {
    query: String,
}

// ---- Proposal param structs (ADR-046) ----

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposeParams {
    title: String,
    description: String,
    changeset: Value,
    #[serde(default)]
    reviewers: Vec<String>,
    expiry: Option<i64>,
    parent_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewParams {
    proposal_id: String,
    decision: String,
    comment: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WithdrawParams {
    proposal_id: String,
    rationale: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListProposalsParams {
    status: Option<String>,
    proposer: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

// ---- Helpers ----

/// Resolve an entity name to its UUID.
///
/// Strategy (for issue #65):
/// 1. Exact case-insensitive name match — returns the entity's UUID.
/// 2. If 0 matches: `NotFound("entity not found: '{name}'")`
/// 3. If multiple matches: `Ambiguous("ambiguous name '{name}': found N entities [id1, id2, ...]")`
///
/// Only searches `entity` substrate (notes and edges don't have meaningful
/// user-facing names in the same sense).
async fn resolve_name_async(
    name: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    // Use EntityFilter.name_prefix with the full name to do an exact match.
    // The DB implements `name LIKE '?%'` so we get back all names that start
    // with `name`. We then filter to exact (case-insensitive) matches.
    let filter = EntityFilter {
        name_prefix: Some(name.to_string()),
        ..Default::default()
    };
    let page = runtime
        .entities(token)?
        .query_entities(
            token.namespace().as_str(),
            filter,
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .map_err(RuntimeError::Storage)?;

    let name_lower = name.to_ascii_lowercase();
    let exact: Vec<_> = page
        .items
        .into_iter()
        .filter(|e| e.name.to_ascii_lowercase() == name_lower && e.deleted_at.is_none())
        .collect();

    match exact.len() {
        0 => Err(RuntimeError::NotFound(format!(
            "entity not found: {name:?}"
        ))),
        1 => Ok(exact[0].id),
        n => {
            let ids: Vec<String> = exact
                .iter()
                .map(|e| e.id.to_string()[..8].to_string())
                .collect();
            Err(RuntimeError::Ambiguous(format!(
                "ambiguous name {name:?}: found {n} entities [{}]",
                ids.join(", ")
            )))
        }
    }
}

/// Resolve a string to a UUID for use as an edge endpoint (source or target).
///
/// Resolution order (issue #65):
/// 1. Full UUID string — parse directly.
/// 2. 8+ hex-character prefix — delegate to `runtime.resolve_prefix`.
/// 3. Everything else — treat as an entity name and call `resolve_name_async`.
async fn resolve_uuid_async(
    s: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(s) {
        return Ok(uuid);
    }
    if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        match runtime.resolve_prefix(token, s).await {
            Ok(Some(uuid)) => return Ok(uuid),
            Ok(None) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "no record matches prefix: {s:?}"
                )))
            }
            Err(e) => return Err(e),
        }
    }
    // Fall back to name-based resolution (issue #65).
    resolve_name_async(s, runtime, token).await
}

// ---- Output formatting helpers (issue #66) ----

/// Truncate a UUID string to 8 characters for compact display.
/// Post-process a serialized edge JSON for display.
///
/// Display formatting (short IDs, compact dates) belongs in the CLI/UI layer,
/// not in the MCP response. Returns the value unchanged.
fn format_edge_output(v: Value, _verbose: bool) -> Value {
    v
}

/// Flatten a `get` result to a top-level object (P-H2).
///
/// Previously `get` returned `{"kind": "entity", "data": {...}}`. That shape
/// forces callers to access fields via `result.data.X` — inconsistent with
/// `create` / `list` which return flat objects. The flat shape matches the
/// other verbs and is easier to work with.
///
/// For entities and notes the inner struct already carries a `kind` field
/// (the entity_kind / note_kind — e.g. "concept", "task"), so we simply
/// return the struct directly. For edges and events there is no `kind` field
/// in the struct, so we inject one to preserve discriminability.
///
/// If the inner value is not an object (shouldn't happen in practice) we fall
/// back to the wrapped form to avoid data loss.
fn flatten_get_result(substrate: &str, mut inner: Value) -> Result<Value, RuntimeError> {
    if let Some(obj) = inner.as_object_mut() {
        // Entities/notes: granular `kind` (e.g. "concept", "observation") stays
        // at top level, mirroring `create` and `list` responses. Edges: inject
        // "edge". Events: rename `kind` (EventKind) to `event_kind`, inject
        // substrate label.
        match substrate {
            "edge" => {
                obj.entry("kind".to_string())
                    .or_insert_with(|| serde_json::Value::String("edge".to_string()));
            }
            "event" => {
                if let Some(event_kind) = obj.remove("kind") {
                    obj.insert("event_kind".to_string(), event_kind);
                }
                obj.insert(
                    "kind".to_string(),
                    serde_json::Value::String("event".to_string()),
                );
            }
            _ => {}
        }
        Ok(inner)
    } else {
        Ok(serde_json::json!({"kind": substrate, "data": inner}))
    }
}

/// Remap note response fields so pack-owned lifecycle status is visible at the
/// top level (Option A — ADR-004).
///
/// The storage-layer `Note.status` field carries row-visibility state
/// (`"active"` | `"archived"` | `"deleted"`). Packs that own a note kind can
/// store their own lifecycle status in `properties.status` — e.g. the GTD pack
/// stores `"inbox"` / `"next"` / `"done"` / … for `kind = "task"`.
///
/// When a note kind has a `properties.status` entry we remap:
/// - `status`    → GTD lifecycle value (from `properties.status`)
/// - `lifecycle` → row-visibility value (what was `status`)
///
/// For note kinds without a pack-owned lifecycle (e.g. `"observation"`,
/// `"insight"`, `"memory"`) `properties.status` is absent, so the note is
/// returned unchanged — `status` continues to reflect row-visibility.
///
/// This transform is applied at the response boundary (KG `get`, `list`,
/// `create`) so every path that serializes a raw storage `Note` to MCP output
/// exposes the right semantic field to callers.
fn remap_note_status(mut note_value: Value) -> Value {
    let Some(obj) = note_value.as_object_mut() else {
        return note_value;
    };
    // Only remap when `properties.status` exists — that signals a pack-owned
    // lifecycle.  `kind` is checked as a belt-and-suspenders guard, but the
    // real discriminator is whether properties carries a status field at all.
    let lifecycle_status = obj
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    if let Some(gtd_status) = lifecycle_status {
        // Move the row-visibility value to `lifecycle`.
        if let Some(row_vis) = obj.remove("status") {
            obj.insert("lifecycle".to_string(), row_vis);
        }
        // Surface the pack-owned lifecycle status as the primary `status`.
        obj.insert("status".to_string(), Value::String(gtd_status));
    }
    note_value
}

fn parse_direction(s: Option<&str>) -> Direction {
    match s {
        Some("in") | Some("incoming") => Direction::In,
        Some("both") => Direction::Both,
        Some("out") | Some("outgoing") | None => Direction::Out,
        Some(_) => Direction::Out,
    }
}

/// Merge `dependency_kind` shortcut into `metadata` for `depends_on` edges.
///
/// When `dependency_kind` is provided separately and `metadata` does not already
/// carry the key, the value is injected into the metadata object. This allows
/// callers to write `dependency_kind: "build"` instead of the full
/// `metadata: { "dependency_kind": "build" }` form.
fn merge_entry_metadata(
    metadata: Option<Value>,
    dependency_kind: Option<String>,
) -> Result<Option<Value>, RuntimeError> {
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

fn parse_relation(s: &str) -> Result<EdgeRelation, RuntimeError> {
    s.parse::<EdgeRelation>().map_err(|_| {
        let valid = EdgeRelation::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        RuntimeError::InvalidInput(format!("unknown relation {s:?}; valid: {valid}"))
    })
}

/// Return the valid edge relations for an entity→entity endpoint pair (issue #486).
///
/// Encodes the ADR-002 base allowlist for UX error enrichment — not for
/// enforcement. `"*"` as `src_kind` means "any source entity kind".
/// Returns an empty vec when no base-contract relations exist for the pair.
pub(crate) fn valid_relations_for_entity_pair(src_kind: &str, tgt_kind: &str) -> Vec<&'static str> {
    const RULES: &[(&str, &str, &str)] = &[
        ("concept", "contains", "concept"),
        ("project", "contains", "project"),
        ("project", "contains", "artifact"),
        ("org", "contains", "project"),
        ("org", "contains", "service"),
        ("concept", "part_of", "concept"),
        ("project", "part_of", "project"),
        ("project", "part_of", "org"),
        ("*", "instance_of", "concept"),
        ("service", "instance_of", "project"),
        ("concept", "extends", "concept"),
        ("concept", "variant_of", "concept"),
        ("artifact", "variant_of", "artifact"),
        ("concept", "introduced_by", "document"),
        ("concept", "introduced_by", "person"),
        ("artifact", "introduced_by", "document"),
        ("artifact", "derived_from", "dataset"),
        ("artifact", "derived_from", "document"),
        ("artifact", "derived_from", "project"),
        ("artifact", "derived_from", "artifact"),
        ("document", "precedes", "document"),
        ("dataset", "precedes", "dataset"),
        ("artifact", "precedes", "artifact"),
        ("service", "precedes", "service"),
        ("project", "precedes", "project"),
        ("project", "depends_on", "project"),
        ("service", "depends_on", "project"),
        ("service", "depends_on", "service"),
        ("service", "depends_on", "artifact"),
        ("service", "depends_on", "dataset"),
        ("artifact", "depends_on", "project"),
        ("artifact", "depends_on", "service"),
        ("concept", "enables", "concept"),
        ("service", "enables", "concept"),
        ("dataset", "enables", "concept"),
        ("project", "implements", "concept"),
        ("service", "implements", "concept"),
        ("concept", "competes_with", "concept"),
        ("project", "competes_with", "project"),
        ("service", "competes_with", "service"),
        ("concept", "composed_with", "concept"),
        ("project", "composed_with", "project"),
        ("concept", "supersedes", "concept"),
        ("document", "supersedes", "document"),
        ("artifact", "supersedes", "artifact"),
        ("service", "supersedes", "service"),
        ("dataset", "supersedes", "dataset"),
    ];
    let mut relations: Vec<&'static str> = RULES
        .iter()
        .filter(|(src, _rel, tgt)| (*src == "*" || *src == src_kind) && *tgt == tgt_kind)
        .map(|(_src, rel, _tgt)| *rel)
        .collect();
    relations.sort_unstable();
    relations.dedup();
    relations
}

/// Enrich an "not in allowlist" error with the list of valid relations (issue #486).
///
/// Called when `runtime.link()` returns `InvalidInput` containing the
/// "not in the ADR-002 base endpoint allowlist" sentinel. Fetches entity kinds
/// and appends valid relations. Returns the original message on lookup failure.
pub(crate) async fn enrich_allowlist_error(
    original: &str,
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
) -> String {
    let src_kind = match runtime.get_entity(token, source_id).await {
        Ok(e) => e.kind,
        Err(_) => return original.to_string(),
    };
    let tgt_kind = match runtime.get_entity(token, target_id).await {
        Ok(e) => e.kind,
        Err(_) => return original.to_string(),
    };
    let valid = valid_relations_for_entity_pair(&src_kind, &tgt_kind);
    if valid.is_empty() {
        format!(
            "Invalid relation {:?} for {src_kind}\u{2192}{tgt_kind}. \
             No valid relations exist for {src_kind}\u{2192}{tgt_kind} in the current edge rules.",
            relation.as_str()
        )
    } else {
        format!(
            "Invalid relation {:?} for {src_kind}\u{2192}{tgt_kind}. \
             Valid relations: {}",
            relation.as_str(),
            valid.join(", ")
        )
    }
}

const IMMUTABLE_EVENT_MSG: &str = "events are immutable — create/update/delete are not permitted";

fn immutable_event_error() -> RuntimeError {
    RuntimeError::InvalidInput(IMMUTABLE_EVENT_MSG.into())
}

fn parse_event_outcome(raw: &str) -> Result<EventOutcome, RuntimeError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "success" => Ok(EventOutcome::Success),
        "denied" => Ok(EventOutcome::Denied),
        "error" => Ok(EventOutcome::Error),
        _ => Err(RuntimeError::InvalidInput(format!(
            "unknown outcome {raw:?}; valid: success | denied | error"
        ))),
    }
}

fn parse_event_substrate(raw: &str) -> Result<SubstrateKind, RuntimeError> {
    raw.trim()
        .to_ascii_lowercase()
        .parse::<SubstrateKind>()
        .map_err(|_| {
            RuntimeError::InvalidInput(format!(
                "unknown substrate {raw:?}; valid: note | entity | event"
            ))
        })
}

fn parse_event_kind(raw: &str) -> Result<EventKind, RuntimeError> {
    raw.parse::<EventKind>()
        .map_err(|e| RuntimeError::InvalidInput(format!("unknown event_kind {raw:?}: {e}")))
}

fn event_filter_from_params(
    p: &ListParams,
) -> Result<(EventFilter, Option<EventOutcome>), RuntimeError> {
    let mut verbs = Vec::new();
    if let Some(verb) = &p.verb {
        verbs.push(verb.clone());
    }
    if let Some(more) = &p.verbs {
        verbs.extend(more.clone());
    }

    let substrates = match p.substrate.as_deref() {
        Some(raw) => vec![parse_event_substrate(raw)?],
        None => Vec::new(),
    };

    let outcome = p.outcome.as_deref().map(parse_event_outcome).transpose()?;

    let mut kinds: Vec<EventKind> = Vec::new();
    if let Some(k) = &p.event_kind {
        kinds.push(parse_event_kind(k)?);
    }
    if let Some(ks) = &p.event_kinds {
        for k in ks {
            kinds.push(parse_event_kind(k)?);
        }
    }

    let session_id = p
        .session_id
        .as_deref()
        .map(|s| {
            Uuid::from_str(s)
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid session_id {s:?}: {e}")))
        })
        .transpose()?;

    let observed = p
        .observed
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| {
            Uuid::from_str(s)
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid observed id {s:?}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let selected = p
        .selected
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|s| {
            Uuid::from_str(s)
                .map_err(|e| RuntimeError::InvalidInput(format!("invalid selected id {s:?}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok((
        EventFilter {
            verbs,
            substrates,
            actors: p.actor.clone().into_iter().collect(),
            after: p.since,
            before: p.until,
            kinds,
            session_id,
            observed,
            selected,
            ..EventFilter::default()
        },
        outcome,
    ))
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<Value, RuntimeError> {
    serde_json::to_value(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))
}

fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

/// Post-process an entity or note JSON value to replace `i64` microsecond epoch
/// timestamps with ISO-8601 strings (ADR-045 §5 handler invariant — C1 fix).
///
/// Applies to the fields `created_at`, `updated_at`, `deleted_at`, and
/// `expires_at` when they are JSON integer values.  `expires_at` is defined on
/// the note substrate (`crates/khive-storage/src/note.rs`) and must be
/// normalized before any note response crosses the MCP boundary.  String values
/// (already converted) and `null` values are left unchanged.  Other fields are
/// unaffected.  Adding `expires_at` here is harmless for entity responses
/// because the helper only rewrites keys that are actually present.
fn normalize_entity_timestamps(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        for field in &["created_at", "updated_at", "deleted_at", "expires_at"] {
            if let Some(val) = obj.get_mut(*field) {
                if let Some(micros) = val.as_i64() {
                    *val = Value::String(micros_to_iso(micros));
                }
            }
        }
    }
    v
}

/// Apply `normalize_entity_timestamps` to every element of a JSON array,
/// or to the value itself if it is already an object.
fn normalize_entity_timestamps_array(v: Value) -> Value {
    match v {
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_entity_timestamps).collect())
        }
        other => normalize_entity_timestamps(other),
    }
}

/// Timestamp key names that must be converted to ISO-8601 strings at the MCP
/// boundary. This set covers all `Timestamp` and `i64` microsecond fields that
/// appear anywhere in event/entity/note/payload JSON — including nested objects
/// and array elements (round-6 recursive fix, ADR-045 §5).
const TIMESTAMP_KEYS: &[&str] = &[
    "created_at",
    "updated_at",
    "deleted_at",
    "expiry",
    "applied_at",
    "withdrawn_at",
    "reviewed_at",
    "completed_at",
    "scheduled_at",
    "expires_at",
    "due",
    "remind_at",
];

/// Recursively walk a `Value` and convert any integer value whose key appears
/// in `TIMESTAMP_KEYS` to an ISO-8601 string.
///
/// - `Value::Object` → for each (key, val): if key is a timestamp key and val
///   is a number, convert it. Then recurse into every value regardless.
/// - `Value::Array` → recurse into every element.
/// - All other variants → no-op.
///
/// Accepts both `u64` (serde repr of `khive_types::Timestamp`) and `i64`
/// (stored epoch microseconds on storage-layer structs). String values and
/// `null` are left unchanged — they are already converted or absent.
fn walk_timestamps(v: &mut Value) {
    match v {
        Value::Object(obj) => {
            for (key, val) in obj.iter_mut() {
                if TIMESTAMP_KEYS.contains(&key.as_str()) {
                    let micros_opt = val.as_u64().map(|n| n as i64).or_else(|| val.as_i64());
                    if let Some(micros) = micros_opt {
                        *val = Value::String(micros_to_iso(micros));
                        // Already a scalar now — no need to recurse into it.
                        continue;
                    }
                }
                walk_timestamps(val);
            }
        }
        Value::Array(arr) => {
            for elem in arr.iter_mut() {
                walk_timestamps(elem);
            }
        }
        _ => {}
    }
}

/// Normalize the `created_at` field on an event JSON object from raw
/// microsecond integer to an ISO-8601 string (ADR-045 §5 handler invariant).
///
/// Round-6: uses `walk_timestamps` to recurse into the entire event value,
/// including arbitrarily-nested payload objects and arrays. This subsumes the
/// round-4 (top-level `created_at`) and round-5 (direct payload children) fixes
/// with a single principled algorithm that applies at any depth.
fn normalize_event_timestamps(mut v: Value) -> Value {
    walk_timestamps(&mut v);
    v
}

/// Apply `normalize_event_timestamps` to every element of a JSON array,
/// or to the value itself if it is already an object.
fn normalize_event_timestamps_array(v: Value) -> Value {
    match v {
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_event_timestamps).collect())
        }
        other => normalize_event_timestamps(other),
    }
}

/// Returns true if `entity_props` contains all key=value pairs from `filter`.
///
/// Both the filter object and the entity's `properties` field must be JSON
/// objects. If the entity has no properties, returns false when the filter is
/// non-empty. String comparisons are case-sensitive.
fn props_match(entity_props: Option<&Value>, filter: &Value) -> bool {
    let required = match filter.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return true, // empty or non-object filter matches everything
    };
    let actual = match entity_props.and_then(Value::as_object) {
        Some(obj) => obj,
        None => return false,
    };
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| av == v))
}

// ---- Handler helpers ----

fn parse_entity_policy(s: &str) -> Result<EntityDedupMergePolicy, RuntimeError> {
    match s {
        "prefer_into" => Ok(EntityDedupMergePolicy::PreferInto),
        "prefer_from" => Ok(EntityDedupMergePolicy::PreferFrom),
        "union" => Ok(EntityDedupMergePolicy::Union),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown strategy {other:?}; use prefer_into | prefer_from | union"
        ))),
    }
}

fn parse_content_strategy(s: &str) -> Result<ContentMergeStrategy, RuntimeError> {
    match s {
        "append" => Ok(ContentMergeStrategy::Append),
        "prefer_into" => Ok(ContentMergeStrategy::PreferInto),
        "prefer_from" => Ok(ContentMergeStrategy::PreferFrom),
        other => Err(RuntimeError::InvalidInput(format!(
            "unknown content_strategy {other:?}; use append | prefer_into | prefer_from"
        ))),
    }
}

async fn ensure_entity_kind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    expected_kind: Option<&str>,
) -> Result<(), RuntimeError> {
    let entity = runtime.get_entity(token, id).await?;
    if let Some(k) = expected_kind {
        if entity.kind != k {
            return Err(RuntimeError::NotFound(format!("{k} {id}")));
        }
    }
    Ok(())
}

async fn ensure_note_kind(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    expected_kind: Option<&str>,
) -> Result<(), RuntimeError> {
    let note = runtime
        .notes(token)?
        .get_note(id)
        .await
        .map_err(RuntimeError::Storage)?
        .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
    if let Some(k) = expected_kind {
        if note.kind != k {
            return Err(RuntimeError::NotFound(format!("{k} {id}")));
        }
    }
    Ok(())
}

fn description_patch(v: Option<Value>) -> Result<Option<Option<String>>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "description must be null or a string, got: {other}"
        ))),
    }
}

fn string_value(v: Option<Value>, field: &str) -> Result<Option<String>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{field} must be a string, got: {other}"
        ))),
    }
}

fn optional_string_patch(
    v: Option<Value>,
    field: &str,
) -> Result<Option<Option<String>>, RuntimeError> {
    match v {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => Ok(Some(Some(s))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{field} must be null or a string, got: {other}"
        ))),
    }
}

/// Serde deserializer for tri-state nullable f64:
/// field absent → outer None, field = null → Some(None), field = number → Some(Some(v)).
fn tri_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<f64>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

// ---- Query result rendering (#286) ----

fn sql_value_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Bool(v) => json!(v),
        SqlValue::Integer(v) => json!(v),
        SqlValue::Float(v) => json!(v),
        SqlValue::Text(v) => json!(v),
        SqlValue::Blob(v) => json!(v),
        SqlValue::Json(v) => v,
        SqlValue::Uuid(v) => json!(v.to_string()),
        SqlValue::Timestamp(v) => json!(v.to_rfc3339()),
    }
}

fn render_query_result(result: QueryResult) -> Value {
    let rows = result
        .rows
        .into_iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for col in row.columns {
                obj.insert(col.name, sql_value_to_json(col.value));
            }
            Value::Object(obj)
        })
        .collect::<Vec<_>>();

    let mut out = serde_json::Map::new();
    out.insert("rows".to_string(), Value::Array(rows));
    if !result.warnings.is_empty() {
        out.insert("warnings".to_string(), json!(result.warnings));
    }
    Value::Object(out)
}

// ---- Handler implementations ----

impl KgPack {
    /// Infer the substrate kind of an existing record from its UUID.
    ///
    /// Called by `handle_update` and `handle_delete` when `kind` is absent
    /// (ADR-014: UUID-only ops). Probes entity → note → edge in order.
    /// Takes no `&VerbRegistry` so it can be `.await`ed freely without
    /// violating the `async_trait` `Send + 'static` bound on `dispatch`.
    async fn infer_kind_from_uuid(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        id_str: &str,
    ) -> Result<KindSpec, RuntimeError> {
        use khive_runtime::Resolved;
        match self.runtime.resolve(token, id).await? {
            Some(Resolved::Entity(_)) => Ok(KindSpec::Entity { specific: None }),
            Some(Resolved::Note(_)) => Ok(KindSpec::Note { specific: None }),
            _ => {
                if self.runtime.get_edge(token, id).await?.is_some() {
                    Ok(KindSpec::Edge)
                } else {
                    Err(RuntimeError::NotFound(format!("not found: {id_str}")))
                }
            }
        }
    }

    pub(crate) async fn handle_create(
        &self,
        token: &NamespaceToken,
        mut params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // Read the discriminator without consuming params (the hook may mutate).
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("create requires 'kind'".into()))?
            .to_string();

        // ue-errors C1 (create): validate user-supplied keys against the
        // declared allowlist BEFORE hook mutation. Hooks inject internal
        // fields (namespace, entity_kind, note_kind, …) AFTER this gate,
        // so unknown user kwargs are caught before any mutation occurs.
        //
        // Allowlist = CreateParams fields + legacy subkind aliases the handler
        // normalises + pack-declared user params (GTD: title, priority, status,
        // assignee, due, start, end, depends_on — consumed by prepare_create).
        const CREATE_USER_KEYS: &[&str] = &[
            // Base KG create fields
            "kind",
            "name",
            "entity_kind",
            "note_kind",
            "entity_type",
            "content",
            "description",
            "tags",
            "properties",
            "salience",
            "annotates",
            // Dedup guard opt-out (issue #487)
            "skip_dedup_check",
            // Linked-edge batch attachment (issue #489)
            "edges",
            // GTD pack user params (create(kind="task", ...))
            "title",
            "priority",
            "status",
            "assignee",
            "due",
            "start",
            "end",
            "depends_on",
        ];
        if let Some(obj) = params.as_object() {
            for key in obj.keys() {
                if !CREATE_USER_KEYS.contains(&key.as_str()) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "create: unknown field `{key}`; allowed: {}",
                        CREATE_USER_KEYS.join(", ")
                    )));
                }
            }
        }

        // Resolve the granular form (`kind="concept"`, `kind="task"`, …) or the
        // legacy substrate-level form (`kind="entity"` + `entity_kind=…`).
        let spec = resolve_kind_spec(&raw_kind, registry)?;

        // Canonicalize the sub-discriminator + look up the kind hook (ADR-030).
        // For entities the hook is rarely used; for notes it's how gtd's `task`
        // kind layers defaults + edges over the shared CRUD path.
        let (sub_kind, hook) = match &spec {
            KindSpec::Entity { specific } => {
                let legacy = params
                    .get("entity_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let canonical = reconcile_specific(
                    specific.clone(),
                    legacy.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?
                .ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "kind=entity requires a specific kind: either kind=<concept|document|dataset|project|person|org|artifact|service> directly, or kind=entity + entity_kind=<…>".into(),
                    )
                })?;
                let hook = registry.find_kind_hook(&canonical);
                (Some(canonical), hook)
            }
            KindSpec::Note { specific } => {
                let legacy = params
                    .get("note_kind")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty());
                let canonical = reconcile_specific(
                    specific.clone(),
                    legacy.as_deref(),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?
                .unwrap_or_else(|| "observation".to_string());
                let hook = registry.find_kind_hook(&canonical);
                (Some(canonical), hook)
            }
            KindSpec::Event => {
                return Err(immutable_event_error());
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "kind=edge is not creatable via `create` — use `link` for edges".into(),
                ));
            }
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "kind=proposal is not creatable via `create` — use `propose` to create a proposal".into(),
                ));
            }
        };

        // Rewrite `kind` to the substrate label so downstream `CreateParams`
        // matching stays substrate-discriminated; granular form is now absorbed.
        if let Some(obj) = params.as_object_mut() {
            obj.insert("kind".into(), json!(spec.substrate_label()));
            // Also normalize the legacy subfield so the hook sees the canonical
            // value when the user passed only the granular form.
            if let Some(ref canonical) = sub_kind {
                match spec {
                    KindSpec::Entity { .. } => {
                        obj.insert("entity_kind".into(), json!(canonical));
                    }
                    KindSpec::Note { .. } => {
                        obj.insert("note_kind".into(), json!(canonical));
                    }
                    KindSpec::Edge | KindSpec::Event | KindSpec::Proposal => {}
                }
            }
        }

        // Propagate the authorized namespace into params so KindHooks can build
        // their own NamespaceToken (hooks don't receive a token directly).
        if let Some(obj) = params.as_object_mut() {
            obj.entry("namespace")
                .or_insert_with(|| json!(token.namespace().as_str()));
        }

        if let Some(ref h) = hook {
            h.prepare_create(&self.runtime, &mut params).await?;
        }

        let p: CreateParams = deser(params.clone())?;
        let skip_dedup = p.skip_dedup_check.unwrap_or(false);

        // Capture entity name + kind before the match consumes `p`; only
        // needed for entity creates with the dedup guard active.
        let dedup_name: Option<String> = if !skip_dedup && p.kind == "entity" {
            p.name.clone()
        } else {
            None
        };
        let dedup_kind: Option<String> = if !skip_dedup && p.kind == "entity" {
            sub_kind.clone()
        } else {
            None
        };

        let (mut response, new_id) = match p.kind.as_str() {
            "entity" => {
                let canonical = sub_kind.clone().expect("entity_kind canonicalized above");
                let name = p.name.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=entity requires 'name'".into())
                })?;
                let tags = p.tags.unwrap_or_default();
                let entity = self
                    .runtime
                    .create_entity(
                        token,
                        &canonical,
                        p.entity_type.as_deref(),
                        &name,
                        p.description.as_deref(),
                        p.properties,
                        tags,
                    )
                    .await?;
                let id = entity.id;
                (normalize_entity_timestamps(to_json(&entity)?), id)
            }
            "note" => {
                let canonical = sub_kind
                    .clone()
                    .unwrap_or_else(|| "observation".to_string());
                let content = p.content.ok_or_else(|| {
                    RuntimeError::InvalidInput("kind=note requires 'content'".into())
                })?;
                let mut annotates = Vec::new();
                for s in p.annotates.unwrap_or_default() {
                    annotates.push(resolve_uuid_async(&s, &self.runtime, token).await?);
                }
                let note = self
                    .runtime
                    .create_note(
                        token,
                        &canonical,
                        p.name.as_deref(),
                        &content,
                        p.salience,
                        p.properties,
                        annotates,
                    )
                    .await?;
                let id = note.id;
                // Normalize microsecond epoch → ISO-8601 before the response
                // reaches the presentation layer (ADR-045 §5 handler invariant,
                // Blocker C1: note create was missing normalization).
                (
                    remap_note_status(normalize_entity_timestamps(to_json(&note)?)),
                    id,
                )
            }
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown kind {other:?}; valid: entity | note"
                )))
            }
        };

        if let Some(ref h) = hook {
            if let Err(e) = h.after_create(&self.runtime, new_id, &params).await {
                tracing::warn!(
                    kind = %sub_kind.as_deref().unwrap_or(""),
                    id = %new_id,
                    error = %e,
                    "kind hook after_create failed (storage write already committed)"
                );
            }
        }

        // Issue #487: advisory dedup guard. After a successful entity create,
        // run a lightweight FTS-only name search to surface similar existing
        // entities. Advisory only — the entity is already committed. Notes are
        // excluded (dedup is meaningful for named entities only).
        if let (Some(ref name), Some(ref kind)) = (&dedup_name, &dedup_kind) {
            const DEDUP_LIMIT: u32 = 3;
            const DEDUP_SCORE_THRESHOLD: f64 = 0.1;
            // +1 so we can discard the just-created entity and still return
            // up to DEDUP_LIMIT results.
            match self
                .runtime
                .hybrid_search(
                    token,
                    name,
                    None,
                    DEDUP_LIMIT + 1,
                    Some(kind.as_str()),
                    None,
                )
                .await
            {
                Ok(hits) => {
                    let similar: Vec<Value> = hits
                        .into_iter()
                        .filter(|h| {
                            h.entity_id != new_id && h.score.to_f64() >= DEDUP_SCORE_THRESHOLD
                        })
                        .take(DEDUP_LIMIT as usize)
                        .map(|h| {
                            json!({
                                "id": h.entity_id.to_string(),
                                "name": h.title,
                                "score": h.score.to_f64(),
                            })
                        })
                        .collect();
                    if !similar.is_empty() {
                        if let Some(obj) = response.as_object_mut() {
                            obj.insert("similar_existing".to_string(), json!(similar));
                        }
                    }
                }
                Err(e) => {
                    // Advisory only — log and continue; the entity is already created.
                    tracing::warn!(
                        id = %new_id,
                        error = %e,
                        "dedup similarity search failed (entity already created)"
                    );
                }
            }
        }

        // Issue #489 — create_linked convenience: attach edges in one round-trip.
        //
        // Process each EdgeSpec using the same validation path as `handle_link`.
        // Failures are collected per-edge; the entity creation is already committed
        // and is never rolled back (partial success is the specified behaviour).
        if let Some(edge_specs) = p.edges {
            if !edge_specs.is_empty() {
                let mut edge_results: Vec<Value> = Vec::with_capacity(edge_specs.len());
                let mut edge_errors: Vec<Value> = Vec::with_capacity(edge_specs.len());
                for (idx, spec) in edge_specs.into_iter().enumerate() {
                    let target =
                        match resolve_uuid_async(&spec.target_id, &self.runtime, token).await {
                            Ok(id) => id,
                            Err(e) => {
                                edge_errors.push(json!({
                                    "index": idx,
                                    "target_id": spec.target_id,
                                    "error": format!("{e}"),
                                }));
                                continue;
                            }
                        };
                    let relation = match parse_relation(&spec.relation) {
                        Ok(r) => r,
                        Err(e) => {
                            edge_errors.push(json!({
                                "index": idx,
                                "target_id": spec.target_id,
                                "relation": spec.relation,
                                "error": format!("{e}"),
                            }));
                            continue;
                        }
                    };
                    let weight = spec.weight.unwrap_or(1.0).clamp(0.0, 1.0);
                    // Symmetric relations use canonical (lower-UUID-first) order.
                    let (source, target) = if relation.is_symmetric() && target < new_id {
                        (target, new_id)
                    } else {
                        (new_id, target)
                    };
                    match self
                        .runtime
                        .link(token, source, target, relation, weight, None)
                        .await
                    {
                        Ok(edge) => match to_json(&edge) {
                            Ok(v) => edge_results.push(v),
                            Err(e) => edge_errors.push(json!({
                                "index": idx,
                                "error": format!("serialize: {e}"),
                            })),
                        },
                        Err(e) => {
                            edge_errors.push(json!({
                                "index": idx,
                                "target_id": spec.target_id,
                                "relation": spec.relation,
                                "error": format!("{e}"),
                            }));
                        }
                    }
                }
                // Augment the response object with edge results.
                let mut out = match response {
                    Value::Object(map) => map,
                    other => {
                        let mut m = serde_json::Map::new();
                        m.insert("entity".to_string(), other);
                        m
                    }
                };
                out.insert("edges".to_string(), Value::Array(edge_results));
                if !edge_errors.is_empty() {
                    out.insert("edge_errors".to_string(), Value::Array(edge_errors));
                }
                return Ok(Value::Object(out));
            }
        }

        Ok(response)
    }

    pub(crate) async fn handle_get(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;

        // ADR-046:299 — `get(id=<proposal_id>)` resolves to the ProposalCreated
        // event payload, not a projection row.  Try to resolve the id against
        // proposals_open first (for UUID/prefix disambiguation), then fetch the
        // ProposalCreated event payload.  Standard substrates win when the same
        // id matches both (shouldn't happen in practice; proposal IDs are fresh UUIDs).

        // Attempt standard UUID resolution for entity/note/edge/event substrates.
        if let Ok(id) = resolve_uuid_async(&p.id, &self.runtime, token).await {
            if let Ok(entity) = self.runtime.get_entity(token, id).await {
                return flatten_get_result(
                    "entity",
                    normalize_entity_timestamps(to_json(&entity)?),
                );
            }

            if let Some(note) = self
                .runtime
                .notes(token)?
                .get_note(id)
                .await
                .map_err(RuntimeError::Storage)?
            {
                if note.namespace == token.namespace().as_str() {
                    let note_val = normalize_entity_timestamps(to_json(&note)?);
                    let remapped = remap_note_status(note_val);
                    return flatten_get_result("note", remapped);
                }
            }

            if let Some(edge) = self.runtime.get_edge(token, id).await? {
                return flatten_get_result("edge", to_json(&edge)?);
            }

            if let Some(event) = self
                .runtime
                .events(token)?
                .get_event(id)
                .await
                .map_err(RuntimeError::Storage)?
            {
                if event.namespace == token.namespace().as_str() {
                    return flatten_get_result(
                        "event",
                        normalize_event_timestamps(to_json(&event)?),
                    );
                }
            }
        }

        // Fall back: resolve as a proposal_id.  ADR-046:299 specifies that
        // get(id=<proposal_id>) resolves to the ProposalCreated event payload.
        if let Some(payload_val) = self.try_get_proposal_payload(token, &p.id).await? {
            return Ok(payload_val);
        }

        Err(RuntimeError::NotFound(format!("not found: {}", p.id)))
    }

    /// Resolve `raw_id` as a proposal ID and return the `ProposalCreated` event payload.
    ///
    /// ADR-046:299 — `get(id=<proposal_id>)` resolves to the `ProposalCreated`
    /// event payload, not a projection row.
    ///
    /// Steps:
    /// 1. Resolve `raw_id` (full UUID or 8-char prefix) against `proposals_open`.
    /// 2. If found (exactly one match), query the event log for the
    ///    `ProposalCreated` event with `payload_proposal_id = <full_uuid>`.
    /// 3. Deserialize the event payload as `ProposalCreatedPayload` and return as JSON.
    ///
    /// Returns `Ok(None)` if no proposal matches.  Returns `Err` only on internal
    /// storage or deserialization failures.
    async fn try_get_proposal_payload(
        &self,
        token: &NamespaceToken,
        raw_id: &str,
    ) -> Result<Option<Value>, RuntimeError> {
        let ns = token.namespace().as_str().to_owned();

        // Step 1: resolve the proposal_id (full UUID or 8-char prefix).
        let (sql_str, params) = if Uuid::from_str(raw_id).is_ok() {
            (
                "SELECT proposal_id FROM proposals_open \
                 WHERE proposal_id = ?1 AND namespace = ?2 LIMIT 1"
                    .to_string(),
                vec![SqlValue::Text(raw_id.to_string()), SqlValue::Text(ns)],
            )
        } else if raw_id.len() >= 8 && raw_id.chars().all(|c| c.is_ascii_hexdigit()) {
            let pattern = format!("{}%", raw_id);
            (
                "SELECT proposal_id FROM proposals_open \
                 WHERE proposal_id LIKE ?1 AND namespace = ?2 LIMIT 2"
                    .to_string(),
                vec![SqlValue::Text(pattern), SqlValue::Text(ns)],
            )
        } else {
            return Ok(None);
        };

        let sql = self.runtime.sql();
        let rows = {
            let mut reader = match sql.reader().await {
                Ok(r) => r,
                Err(e) => return Err(RuntimeError::Storage(e)),
            };
            match reader
                .query_all(SqlStatement {
                    sql: sql_str,
                    params,
                    label: Some("proposals_open.resolve_for_get".into()),
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if e.to_string().contains("no such table") {
                        return Ok(None);
                    }
                    return Err(RuntimeError::Storage(e));
                }
            }
        };

        // Guard against ambiguous prefix — return None so the caller can propagate NotFound.
        if rows.len() != 1 {
            return Ok(None);
        }

        let full_uuid_str = rows[0]
            .get("proposal_id")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                RuntimeError::Internal("proposal_id column missing from proposals_open row".into())
            })?;

        let proposal_uuid = Uuid::from_str(&full_uuid_str).map_err(|e| {
            RuntimeError::Internal(format!("stored proposal_id is not a valid UUID: {e}"))
        })?;

        // Step 2: fetch the ProposalCreated event from the event log.
        let event_store = self.runtime.events(token)?;
        let page = event_store
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::ProposalCreated],
                    payload_proposal_id: Some(proposal_uuid),
                    ..Default::default()
                },
                khive_storage::types::PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .map_err(RuntimeError::Storage)?;

        let event = match page.items.into_iter().next() {
            Some(e) => e,
            None => {
                return Err(RuntimeError::Internal(format!(
                    "ProposalCreated event not found for proposal_id {proposal_uuid}"
                )));
            }
        };

        // Step 3: deserialize the payload and return as JSON.
        // Use from_str (not from_value) so Id128::deserialize works with string-backed data.
        let payload_str = event.payload.to_string();
        let payload: khive_types::ProposalCreatedPayload = serde_json::from_str(&payload_str)
            .map_err(|e| {
                RuntimeError::Internal(format!(
                    "failed to deserialize ProposalCreated payload: {e}"
                ))
            })?;

        let mut result = serde_json::to_value(&payload).map_err(|e| {
            RuntimeError::Internal(format!(
                "failed to re-serialize ProposalCreatedPayload: {e}"
            ))
        })?;

        // Inject a top-level "kind" discriminant so callers can identify the response type.
        if let serde_json::Value::Object(ref mut map) = result {
            map.insert(
                "kind".to_string(),
                serde_json::Value::String("proposal".to_string()),
            );
        }

        Ok(Some(result))
    }

    pub(crate) async fn handle_list(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        // Fast-path: kind=proposal dispatches to the proposals_open projection
        // before deserializing into ListParams, so proposal-specific fields
        // (status, proposer) are handled without polluting ListParams.
        let raw_kind = params
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if raw_kind == "proposal" {
            return self.handle_list_proposals(token, params).await;
        }

        let p: ListParams = deser(params)?;
        let spec = resolve_kind_spec(&p.kind, registry)?;
        match spec {
            KindSpec::Entity { specific } => {
                // CC-2 fix: reject contradicting note_kind when listing entities
                if p.note_kind.as_deref().is_some_and(|s| !s.is_empty()) {
                    return Err(RuntimeError::InvalidInput(
                        "note_kind filter is not valid when kind=entity; use kind=note to list notes".into(),
                    ));
                }
                let kind_filter = reconcile_specific(
                    specific,
                    p.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?;
                let limit = p.limit.unwrap_or(50).min(500);
                let offset = p.offset.unwrap_or(0);
                // CC-2 fix: pass tags_any through EntityFilter so that
                // list(kind="entity", tags=[X]) actually filters by tag.
                // When tags are present we call query_entities directly; otherwise
                // we keep the original list_entities path (no tags: empty filter).
                let entities = if let Some(ref tag_list) = p.tags {
                    if tag_list.is_empty() {
                        self.runtime
                            .list_entities(
                                token,
                                kind_filter.as_deref(),
                                p.entity_type.as_deref(),
                                limit,
                                offset,
                            )
                            .await?
                    } else {
                        use khive_storage::types::PageRequest;
                        let filter = EntityFilter {
                            kinds: kind_filter
                                .as_deref()
                                .map(|k| vec![k.to_string()])
                                .unwrap_or_default(),
                            entity_types: p
                                .entity_type
                                .as_deref()
                                .map(|t| vec![t.to_string()])
                                .unwrap_or_default(),
                            tags_any: tag_list.clone(),
                            ..Default::default()
                        };
                        let page = self
                            .runtime
                            .entities(token)?
                            .query_entities(
                                token.namespace().as_str(),
                                filter,
                                PageRequest {
                                    offset: offset.into(),
                                    limit,
                                },
                            )
                            .await
                            .map_err(RuntimeError::Storage)?;
                        page.items
                    }
                } else {
                    self.runtime
                        .list_entities(
                            token,
                            kind_filter.as_deref(),
                            p.entity_type.as_deref(),
                            limit,
                            offset,
                        )
                        .await?
                };
                // Normalize i64 microsecond timestamps to ISO-8601 strings
                // (ADR-045 §5 handler invariant — C1 fix).
                Ok(normalize_entity_timestamps_array(to_json(&entities)?))
            }
            KindSpec::Edge => {
                let source_id = match p.source_id.as_deref() {
                    Some(s) => Some(resolve_uuid_async(s, &self.runtime, token).await?),
                    None => None,
                };
                let target_id = match p.target_id.as_deref() {
                    Some(s) => Some(resolve_uuid_async(s, &self.runtime, token).await?),
                    None => None,
                };
                let relations: Vec<EdgeRelation> = p
                    .relations
                    .unwrap_or_default()
                    .iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let filter = EdgeListFilter {
                    source_id,
                    target_id,
                    relations,
                    min_weight: p.min_weight,
                    max_weight: p.max_weight,
                };
                let limit = p.limit.unwrap_or(100);
                let edges = self.runtime.list_edges(token, filter, limit).await?;
                to_json(&edges)
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                let limit = p.limit.unwrap_or(20).min(200);
                let offset = p.offset.unwrap_or(0);

                // Determine whether any message-specific property filters are active.
                // These are not pushed to SQL (the NoteStore only filters by namespace +
                // kind); they are applied in-memory after retrieval.
                let has_msg_filter = p.thread_id.is_some()
                    || p.direction.is_some()
                    || p.from.is_some()
                    || p.to.is_some()
                    || p.read.is_some();

                // Normalise a thread_id for comparison: accept either the 8-char
                // short prefix or the full 36-char UUID form.
                let thread_id_filter = p.thread_id.as_deref();
                let direction_filter = p.direction.as_deref();
                let from_filter = p.from.as_deref();
                let to_filter = p.to.as_deref();
                let read_filter = p.read;

                // When message filters are active, use a paginated scan so that matching
                // rows are never lost behind a deep backlog of non-matching messages.
                // Total scan is capped at MAX_SCAN_TOTAL to avoid pathological performance
                // on very large note stores (e.g. 1M+ messages).
                // For deep mailboxes, prefer comm.inbox (no cap) or comm.thread (thread-indexed).
                // See ADR-040 §"Message-filter scan cap" for rationale and alternatives.
                const PAGE_SIZE: u32 = 200;
                const MAX_SCAN_TOTAL: u32 = 10_000;

                let notes: Vec<_> = if has_msg_filter {
                    let mut collected: Vec<_> = Vec::new();
                    let mut db_offset: u32 = 0;
                    let target_after_skip = offset as usize + limit as usize;
                    loop {
                        let remaining_scan =
                            MAX_SCAN_TOTAL.saturating_sub(db_offset).min(PAGE_SIZE);
                        if remaining_scan == 0 {
                            break;
                        }
                        let page = self
                            .runtime
                            .list_notes(token, kind_filter.as_deref(), remaining_scan, db_offset)
                            .await?;
                        let fetched = page.len() as u32;
                        for n in page {
                            if n.deleted_at.is_some() {
                                continue;
                            }
                            let props = n.properties.as_ref();
                            let passes = (|| {
                                if let Some(wanted_thread) = thread_id_filter {
                                    let stored = match props
                                        .and_then(|p| p.get("thread_id"))
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.is_empty())
                                    {
                                        Some(s) => s,
                                        None => return false,
                                    };
                                    let matches = stored == wanted_thread
                                        || (stored.len() >= 8
                                            && wanted_thread.len() >= 8
                                            && stored[..8] == wanted_thread[..8]);
                                    if !matches {
                                        return false;
                                    }
                                }
                                if let Some(wanted_dir) = direction_filter {
                                    let stored = props
                                        .and_then(|p| p.get("direction"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if stored != wanted_dir {
                                        return false;
                                    }
                                }
                                if let Some(wanted_from) = from_filter {
                                    let stored = props
                                        .and_then(|p| p.get("from"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if stored != wanted_from {
                                        return false;
                                    }
                                }
                                if let Some(wanted_to) = to_filter {
                                    let stored = props
                                        .and_then(|p| p.get("to"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("");
                                    if stored != wanted_to {
                                        return false;
                                    }
                                }
                                if let Some(wanted_read) = read_filter {
                                    let stored = props
                                        .and_then(|p| p.get("read"))
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false);
                                    if stored != wanted_read {
                                        return false;
                                    }
                                }
                                true
                            })();
                            if passes {
                                collected.push(n);
                                if collected.len() >= target_after_skip {
                                    break;
                                }
                            }
                        }
                        if collected.len() >= target_after_skip || fetched < PAGE_SIZE {
                            break;
                        }
                        db_offset += fetched;
                    }
                    collected
                } else {
                    self.runtime
                        .list_notes(token, kind_filter.as_deref(), limit, offset)
                        .await?
                };

                // notes is already the correct filtered+paged slice in both paths:
                // - has_msg_filter=true:  paginated scan above yielded up to target_after_skip
                //   matching rows; apply skip+take here to honour offset within those results.
                // - has_msg_filter=false: list_notes was called with the correct limit/offset.
                let remapped: Vec<Value> = if has_msg_filter {
                    notes
                        .into_iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .map(|n| {
                            to_json(&n)
                                .map(normalize_entity_timestamps)
                                .map(remap_note_status)
                                .unwrap_or_else(|_| serde_json::json!({}))
                        })
                        .collect()
                } else {
                    notes
                        .iter()
                        .filter(|n| n.deleted_at.is_none())
                        .map(|n| {
                            to_json(n)
                                .map(normalize_entity_timestamps)
                                .map(remap_note_status)
                                .unwrap_or_else(|_| serde_json::json!({}))
                        })
                        .collect()
                };
                to_json(&remapped)
            }
            KindSpec::Proposal => unreachable!("kind=proposal fast-pathed before deser"),
            KindSpec::Event => {
                let limit = p.limit.unwrap_or(100).clamp(1, 1000);
                let offset = p.offset.unwrap_or(0);
                let (filter, outcome) = event_filter_from_params(&p)?;

                if let Some(wanted_outcome) = outcome {
                    let mut items = Vec::new();
                    let mut skipped = 0u32;
                    let mut raw_offset = 0u32;
                    let scan_ceiling = offset.saturating_add(limit).saturating_mul(20);

                    while (items.len() as u32) < limit {
                        let remaining = scan_ceiling.saturating_sub(raw_offset);
                        if remaining == 0 {
                            break;
                        }
                        let batch_size = 100u32.min(remaining);
                        let page = self
                            .runtime
                            .list_events(
                                token,
                                filter.clone(),
                                PageRequest {
                                    limit: batch_size,
                                    offset: raw_offset.into(),
                                },
                            )
                            .await?;
                        let batch_len = page.items.len() as u32;
                        if batch_len == 0 {
                            break;
                        }
                        raw_offset = raw_offset.saturating_add(batch_len);
                        let eof = batch_len < batch_size;

                        for event in page.items {
                            if event.outcome != wanted_outcome {
                                continue;
                            }
                            if skipped < offset {
                                skipped += 1;
                                continue;
                            }
                            items.push(event);
                            if (items.len() as u32) >= limit {
                                break;
                            }
                        }

                        if eof {
                            break;
                        }
                    }
                    Ok(normalize_event_timestamps_array(to_json(&items)?))
                } else {
                    let page = self
                        .runtime
                        .list_events(
                            token,
                            filter,
                            PageRequest {
                                limit,
                                offset: offset.into(),
                            },
                        )
                        .await?;
                    Ok(normalize_event_timestamps_array(to_json(&page.items)?))
                }
            }
        }
    }

    pub(crate) async fn handle_update(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: UpdateParams = deser(params)?;
        // ue-kg-deep C3 fix: entity_kind is immutable after creation. Silently
        // discarding this field was the bug — now we surface an explicit error.
        if p.entity_kind.is_some() {
            return Err(RuntimeError::InvalidInput(
                "entity_kind is immutable; to change kind, delete then re-create the entity, or use merge() if this is a deduplication correction".into(),
            ));
        }
        // Resolve `kind` with registry BEFORE the first .await so that the
        // registry borrow is provably dead at all yield points (mirrors the
        // pattern used in handle_create / handle_list).  ADR-014: when `kind`
        // is absent, the substrate is inferred from the UUID via
        // `infer_kind_from_uuid` (which takes no `registry`).
        let explicit_spec: Option<KindSpec> = if let Some(k) = p.kind.as_deref() {
            Some(resolve_kind_spec(k, registry)?)
        } else {
            None
        };
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let spec: KindSpec = match explicit_spec {
            Some(s) => s,
            None => self.infer_kind_from_uuid(token, id, &p.id).await?,
        };

        match spec {
            KindSpec::Entity { specific } => {
                let entity = self.runtime.get_entity(token, id).await?;
                if specific.as_ref().is_some_and(|k| entity.kind != *k) {
                    return Err(RuntimeError::NotFound(format!("entity {}", p.id)));
                }
                let patch = EntityPatch {
                    name: string_value(p.name, "name")?,
                    description: description_patch(p.description)?,
                    properties: p.properties,
                    tags: p.tags,
                };
                Ok(normalize_entity_timestamps(to_json(
                    &self.runtime.update_entity(token, id, patch).await?,
                )?))
            }
            KindSpec::Edge => {
                let relation = p.relation.as_deref().map(parse_relation).transpose()?;
                let patch = EdgePatch {
                    relation,
                    weight: p.weight,
                    properties: p.properties,
                };
                to_json(&self.runtime.update_edge(token, id, patch).await?)
            }
            KindSpec::Note { specific } => {
                let note = self
                    .runtime
                    .notes(token)?
                    .get_note(id)
                    .await
                    .map_err(RuntimeError::Storage)?;
                if note
                    .as_ref()
                    .is_none_or(|n| specific.as_ref().is_some_and(|k| n.kind != *k))
                {
                    return Err(RuntimeError::NotFound(format!("note {}", p.id)));
                }
                let patch = NotePatch::new(
                    optional_string_patch(p.name, "name")?,
                    p.content,
                    p.salience,
                    p.decay_factor,
                    p.properties,
                );
                Ok(normalize_entity_timestamps(to_json(
                    &self.runtime.update_note(token, id, patch).await?,
                )?))
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "proposal events are immutable — use `withdraw` to rescind a proposal".into(),
            )),
        }
    }

    pub(crate) async fn handle_delete(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteParams = deser(params)?;
        // Resolve `kind` with registry BEFORE the first .await — same pattern
        // as handle_update. When `kind` is absent, substrate is inferred from
        // the UUID after the await (ADR-014).
        let explicit_spec: Option<KindSpec> = if let Some(k) = p.kind.as_deref() {
            Some(resolve_kind_spec(k, registry)?)
        } else {
            None
        };
        let id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let spec: KindSpec = match explicit_spec {
            Some(s) => s,
            None => self.infer_kind_from_uuid(token, id, &p.id).await?,
        };

        match spec {
            KindSpec::Entity { specific } => {
                if let Some(ref expected) = specific {
                    let entity = self.runtime.get_entity(token, id).await?;
                    if entity.kind != *expected {
                        return Err(RuntimeError::NotFound(format!("{} {}", expected, p.id)));
                    }
                }
                let deleted = self
                    .runtime
                    .delete_entity(token, id, p.hard.unwrap_or(false))
                    .await?;
                if !deleted {
                    return Err(RuntimeError::NotFound(format!("entity {}", p.id)));
                }
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": p.kind }))
            }
            KindSpec::Note { specific } => {
                if let Some(ref expected) = specific {
                    let note = self
                        .runtime
                        .notes(token)?
                        .get_note(id)
                        .await
                        .map_err(RuntimeError::Storage)?;
                    if note.as_ref().is_none_or(|n| n.kind != *expected) {
                        return Err(RuntimeError::NotFound(format!("{} {}", expected, p.id)));
                    }
                }
                let deleted = self
                    .runtime
                    .delete_note(token, id, p.hard.unwrap_or(false))
                    .await?;
                if !deleted {
                    return Err(RuntimeError::NotFound(format!("note {}", p.id)));
                }
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": p.kind }))
            }
            KindSpec::Edge => {
                let deleted = self
                    .runtime
                    .delete_edge(token, id, p.hard.unwrap_or(false))
                    .await?;
                to_json(&serde_json::json!({ "deleted": deleted, "id": p.id, "kind": "edge" }))
            }
            KindSpec::Event => Err(immutable_event_error()),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "proposal events are immutable — use `withdraw` to rescind a proposal".into(),
            )),
        }
    }

    pub(crate) async fn handle_merge(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        let into_id = resolve_uuid_async(&p.into_id, &self.runtime, token).await?;
        let from_id = resolve_uuid_async(&p.from_id, &self.runtime, token).await?;
        let raw_kind = p.kind.as_deref().unwrap_or("entity");
        let spec = resolve_kind_spec(raw_kind, registry)?;
        let policy = parse_entity_policy(p.strategy.as_deref().unwrap_or("prefer_into"))?;
        let content_strategy =
            parse_content_strategy(p.content_strategy.as_deref().unwrap_or("append"))?;
        let dry_run = p.dry_run.unwrap_or(false);

        let summary: MergeSummary = match spec {
            KindSpec::Entity { specific } => {
                ensure_entity_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_entity_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                // C2 fix: reject cross-kind merge (concept+project, etc.) before
                // any writes. Fetching both entities here is cheap — merge_entity_sql
                // will fetch them again inside the transaction, but the early guard
                // gives a clear error message before any SQL side-effects occur.
                let into_entity = self.runtime.get_entity(token, into_id).await?;
                let from_entity = self.runtime.get_entity(token, from_id).await?;
                if into_entity.kind != from_entity.kind {
                    return Err(RuntimeError::InvalidInput(format!(
                        "cannot merge entities of different kinds: into={} ({}), from={} ({})",
                        into_id, into_entity.kind, from_id, from_entity.kind
                    )));
                }
                self.runtime
                    .merge_entity(token, into_id, from_id, policy, dry_run)
                    .await?
            }
            KindSpec::Note { specific } => {
                ensure_note_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_note_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                self.runtime
                    .merge_note(token, into_id, from_id, policy, content_strategy, dry_run)
                    .await?
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "merge(kind=\"edge\") is unsupported".into(),
                ))
            }
            KindSpec::Event => return Err(immutable_event_error()),
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "proposal events are immutable and cannot be merged".into(),
                ))
            }
        };
        to_json(&summary)
    }

    pub(crate) async fn handle_search(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: SearchParams = deser(params)?;
        let limit = p.limit.unwrap_or(10).min(100);
        let spec = resolve_kind_spec(&p.kind, registry)?;
        match spec {
            KindSpec::Entity { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.entity_kind.as_deref(),
                    |s| canonical_entity_kind(s, registry),
                    "entity_kind",
                )?;
                // When a properties filter is active, pull extra candidates so
                // that filtering doesn't leave fewer results than `limit`.
                let props_filter = p.properties.as_ref().and_then(|v| {
                    if v.as_object().is_some_and(|m| !m.is_empty()) {
                        Some(v)
                    } else {
                        None
                    }
                });
                let search_limit = if props_filter.is_some() {
                    (limit * 4).min(100)
                } else {
                    limit
                };
                let hits = self
                    .runtime
                    .hybrid_search(
                        token,
                        &p.query,
                        None,
                        search_limit,
                        kind_filter.as_deref(),
                        p.entity_type.as_deref(),
                    )
                    .await?;

                // Fetch entity metadata for every hit so the response can carry
                // `entity_kind` per #160. When a properties filter is also active
                // (#163), reuse the same fetch for filtering.
                let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();
                let entity_meta: HashMap<Uuid, (String, Option<Value>)> = if candidate_ids
                    .is_empty()
                {
                    HashMap::new()
                } else {
                    let entities_page = self
                        .runtime
                        .entities(token)?
                        .query_entities(
                            token.namespace().as_str(),
                            EntityFilter {
                                ids: candidate_ids,
                                ..EntityFilter::default()
                            },
                            PageRequest {
                                offset: 0u64,
                                limit: hits.len() as u32,
                            },
                        )
                        .await
                        .map_err(RuntimeError::Storage)?;
                    entities_page
                        .items
                        .into_iter()
                        .map(|e| (e.id, (e.kind, e.properties)))
                        .collect()
                };

                // Apply properties post-filter if requested.
                let filtered_hits = if let Some(pf) = props_filter {
                    hits.into_iter()
                        .filter(|h| {
                            entity_meta
                                .get(&h.entity_id)
                                .is_some_and(|(_, props)| props_match(props.as_ref(), pf))
                        })
                        .take(limit as usize)
                        .collect::<Vec<_>>()
                } else {
                    hits
                };

                // C4 fix: apply min_score floor when the caller specifies one.
                // No server-side default — RRF rank-1 scores ≈ 0.016 so any
                // non-trivial default would silently hide valid matches. Callers
                // who want to suppress noise should pass min_score explicitly.
                let score_floor = p.min_score.unwrap_or(0.0).max(0.0);
                let result: Vec<Value> = filtered_hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= score_floor)
                    .map(|h| {
                        // #160: include entity_kind so agents can distinguish hit
                        // kinds without an extra get() call.
                        let entity_kind = entity_meta.get(&h.entity_id).map(|(k, _)| k.as_str());
                        serde_json::json!({
                            "id": h.entity_id.to_string(),
                            "entity_kind": entity_kind,
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Note { specific } => {
                let kind_filter = reconcile_specific(
                    specific,
                    p.note_kind.as_deref().filter(|s| !s.is_empty()),
                    |s| canonical_note_kind(s, registry),
                    "note_kind",
                )?;
                let hits = self
                    .runtime
                    .search_notes(
                        token,
                        &p.query,
                        None,
                        limit,
                        kind_filter.as_deref(),
                        p.include_superseded.unwrap_or(false),
                    )
                    .await?;

                // #160 (note half): fetch note records so the response can
                // carry note_kind. NoteSearchHit doesn't expose it directly.
                let note_kinds: HashMap<Uuid, String> = if hits.is_empty() {
                    HashMap::new()
                } else {
                    let note_store = self.runtime.notes(token)?;
                    let mut map = HashMap::new();
                    for h in &hits {
                        if let Ok(Some(n)) = note_store.get_note(h.note_id).await {
                            map.insert(h.note_id, n.kind);
                        }
                    }
                    map
                };

                // C4 fix: apply min_score floor when the caller specifies one.
                let score_floor = p.min_score.unwrap_or(0.0).max(0.0);
                let result: Vec<Value> = hits
                    .iter()
                    .filter(|h| h.score.to_f64() >= score_floor)
                    .map(|h| {
                        serde_json::json!({
                            "id": h.note_id.to_string(),
                            "note_kind": note_kinds.get(&h.note_id),
                            "score": h.score.to_f64(),
                            "title": h.title,
                            "snippet": h.snippet,
                        })
                    })
                    .collect();
                to_json(&result)
            }
            KindSpec::Edge => Err(RuntimeError::InvalidInput(
                "search does not support kind=edge — use `list(kind=\"edge\", ...)` for edge browsing".into(),
            )),
            KindSpec::Event => Err(RuntimeError::InvalidInput(
                "search does not support kind=event — use `list(kind=\"event\", ...)` for event browsing".into(),
            )),
            KindSpec::Proposal => Err(RuntimeError::InvalidInput(
                "search does not support kind=proposal — use `list(kind=\"proposal\", ...)` for proposal browsing".into(),
            )),
        }
    }

    pub(crate) async fn handle_link(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: LinkParams = deser(params)?;
        let verbose = p.verbose.unwrap_or(false);

        if let Some(entries) = p.links {
            let attempted = entries.len();
            if attempted > 1000 {
                return Err(RuntimeError::InvalidInput(
                    "bulk link limited to 1000 entries per request".into(),
                ));
            }
            let atomic = p.atomic.unwrap_or(true);
            if atomic {
                let mut specs = Vec::with_capacity(attempted);
                let mut seen = std::collections::HashSet::new();
                let mut skipped = 0usize;
                for entry in entries {
                    let source = resolve_uuid_async(&entry.source_id, &self.runtime, token).await?;
                    let target = resolve_uuid_async(&entry.target_id, &self.runtime, token).await?;
                    let relation = parse_relation(&entry.relation)?;
                    let (source, target) = if relation.is_symmetric() && target < source {
                        (target, source)
                    } else {
                        (source, target)
                    };
                    let key = format!("{source}::{target}::{}", relation.as_str());
                    if !seen.insert(key) {
                        skipped += 1;
                        continue;
                    }
                    let weight = entry.weight.unwrap_or(1.0).clamp(0.0, 1.0);
                    let metadata = merge_entry_metadata(entry.metadata, entry.dependency_kind)?;
                    specs.push(LinkSpec {
                        namespace: Some(token.namespace().as_str().to_owned()),
                        source_id: source,
                        target_id: target,
                        relation,
                        weight,
                        metadata,
                    });
                }
                let edges = self.runtime.link_many(token, specs).await?;
                let mut resp = serde_json::json!({
                    "attempted": attempted,
                    "created": edges.len(),
                    "skipped": skipped,
                    "failed": 0,
                });
                if verbose {
                    resp["edges"] = serde_json::to_value(&edges)
                        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;
                }
                return to_json(&resp);
            } else {
                let mut results: Vec<Value> = Vec::new();
                let mut error_list: Vec<Value> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let mut skipped = 0usize;
                for (idx, entry) in entries.into_iter().enumerate() {
                    let source =
                        match resolve_uuid_async(&entry.source_id, &self.runtime, token).await {
                            Ok(id) => id,
                            Err(e) => {
                                error_list.push(json!({"index": idx, "error": format!("{e}")}));
                                continue;
                            }
                        };
                    let target =
                        match resolve_uuid_async(&entry.target_id, &self.runtime, token).await {
                            Ok(id) => id,
                            Err(e) => {
                                error_list.push(json!({"index": idx, "error": format!("{e}")}));
                                continue;
                            }
                        };
                    let relation = match parse_relation(&entry.relation) {
                        Ok(r) => r,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    let (source, target) = if relation.is_symmetric() && target < source {
                        (target, source)
                    } else {
                        (source, target)
                    };
                    let key = format!("{source}::{target}::{}", relation.as_str());
                    if !seen.insert(key) {
                        skipped += 1;
                        continue;
                    }
                    let weight = entry.weight.unwrap_or(1.0).clamp(0.0, 1.0);
                    let metadata = match merge_entry_metadata(entry.metadata, entry.dependency_kind)
                    {
                        Ok(m) => m,
                        Err(e) => {
                            error_list.push(json!({"index": idx, "error": format!("{e}")}));
                            continue;
                        }
                    };
                    match self
                        .runtime
                        .link(token, source, target, relation, weight, metadata)
                        .await
                    {
                        Ok(edge) => results.push(to_json(&edge)?),
                        Err(e) => error_list.push(json!({"index": idx, "error": format!("{e}")})),
                    }
                }
                let mut resp = serde_json::json!({
                    "attempted": attempted,
                    "created": results.len(),
                    "skipped": skipped,
                    "failed": error_list.len(),
                    "errors": error_list,
                });
                if verbose {
                    resp["edges"] = serde_json::Value::Array(results);
                }
                return to_json(&resp);
            }
        }

        // Singleton path.
        let source_id_str = p.source_id.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires source_id (or links for bulk)".into())
        })?;
        let target_id_str = p.target_id.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires target_id (or links for bulk)".into())
        })?;
        let relation_str = p.relation.ok_or_else(|| {
            RuntimeError::InvalidInput("link requires relation (or links for bulk)".into())
        })?;
        let source = resolve_uuid_async(&source_id_str, &self.runtime, token).await?;
        let target = resolve_uuid_async(&target_id_str, &self.runtime, token).await?;
        let weight = p.weight.unwrap_or(1.0).clamp(0.0, 1.0);
        let relation = parse_relation(&relation_str)?;
        let metadata = merge_entry_metadata(p.metadata, p.dependency_kind)?;

        let edge = match self
            .runtime
            .link(token, source, target, relation, weight, metadata)
            .await
        {
            Ok(e) => e,
            Err(RuntimeError::InvalidInput(ref msg))
                if msg.contains("not in the ADR-002 base endpoint allowlist") =>
            {
                let enriched =
                    enrich_allowlist_error(msg, &self.runtime, token, source, target, relation)
                        .await;
                return Err(RuntimeError::InvalidInput(enriched));
            }
            Err(e) => return Err(e),
        };
        let mut raw = to_json(&edge)?;
        // K-C1: for symmetric relations the runtime stores a canonical (lower-UUID-first)
        // endpoint order. Restore the caller's original positions in the response so the
        // caller sees exactly what they specified, not the internal storage order.
        if relation.is_symmetric() {
            if let Some(obj) = raw.as_object_mut() {
                obj.insert("source_id".to_string(), json!(source.to_string()));
                obj.insert("target_id".to_string(), json!(target.to_string()));
            }
        }
        Ok(format_edge_output(raw, verbose))
    }

    pub(crate) async fn handle_neighbors(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: NeighborsParams = deser(params)?;
        let node_id = resolve_uuid_async(&p.id, &self.runtime, token).await?;
        let direction = parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let hits = self
            .runtime
            .neighbors_with_query(
                token,
                node_id,
                NeighborQuery {
                    direction,
                    relations,
                    limit: p.limit,
                    min_weight: p.min_weight,
                },
            )
            .await?;
        to_json(&hits)
    }

    pub(crate) async fn handle_traverse(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: TraverseParams = deser(params)?;
        let mut roots = Vec::with_capacity(p.roots.len());
        for s in &p.roots {
            roots.push(resolve_uuid_async(s, &self.runtime, token).await?);
        }
        let direction = parse_direction(p.direction.as_deref());
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let options = TraversalOptions {
            max_depth: p.max_depth.unwrap_or(3),
            direction,
            relations,
            min_weight: p.min_weight,
            limit: p.limit,
        };
        let request = TraversalRequest {
            roots,
            options,
            include_roots: p.include_roots.unwrap_or(true),
        };
        let paths = self.runtime.traverse(token, request).await?;
        to_json(&paths)
    }

    pub(crate) async fn handle_query(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: QueryParams = deser(params)?;
        let result = self.runtime.query_with_metadata(token, &p.query).await?;
        Ok(render_query_result(result))
    }

    // ---- Proposal verbs (ADR-046) ----

    /// Resolve a proposal_id string (full UUID or 8-char prefix) to a full UUID.
    ///
    /// H1: `review` and `withdraw` previously called `Uuid::from_str` directly,
    /// which rejects 8-char short IDs with "expected length 32 for simple format,
    /// found 8".  Every other verb accepts short IDs via `resolve_uuid_async`.
    /// Proposal IDs live in `proposals_open`, not the four tables that
    /// `resolve_prefix` searches, so we need a dedicated prefix query here.
    async fn resolve_proposal_uuid(
        &self,
        token: &NamespaceToken,
        raw: &str,
    ) -> Result<Uuid, RuntimeError> {
        // Fast path: already a full UUID.
        if let Ok(uuid) = Uuid::from_str(raw) {
            return Ok(uuid);
        }
        // Prefix resolution: require at least 8 hex chars to avoid ambiguity.
        if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
            let ns = token.namespace().as_str().to_owned();
            let pattern = format!("{}%", raw);
            let sql = self.runtime.sql();
            let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
            let rows = reader
                .query_all(SqlStatement {
                    sql: "SELECT proposal_id FROM proposals_open \
                          WHERE proposal_id LIKE ?1 AND namespace = ?2 LIMIT 2"
                        .to_string(),
                    params: vec![SqlValue::Text(pattern), SqlValue::Text(ns)],
                    label: Some("proposals_open.resolve_prefix".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;

            let ids: Vec<String> = rows
                .into_iter()
                .filter_map(|row| {
                    row.get("proposal_id").and_then(|v| {
                        if let SqlValue::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                })
                .collect();

            return match ids.len() {
                0 => Err(RuntimeError::NotFound(format!(
                    "no proposal matches prefix: {raw:?}"
                ))),
                1 => Uuid::from_str(&ids[0]).map_err(|e| {
                    RuntimeError::Internal(format!("stored proposal_id is invalid: {e}"))
                }),
                _ => Err(RuntimeError::InvalidInput(format!(
                    "ambiguous proposal prefix {raw:?}: matches multiple proposals; use full UUID"
                ))),
            };
        }
        Err(RuntimeError::InvalidInput(format!(
            "invalid proposal_id {raw:?}: must be a full UUID or 8-char hex prefix"
        )))
    }

    /// `propose` — commissive verb. Emits a `ProposalCreated` event and inserts
    /// a row into the `proposals_open` projection table.
    pub(crate) async fn handle_propose(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ProposeParams = deser(params)?;
        if p.title.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "propose requires a non-empty 'title'".into(),
            ));
        }
        if p.description.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "propose requires a non-empty 'description'".into(),
            ));
        }

        let _changeset: ProposalChangeset = serde_json::from_value(p.changeset.clone())
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid changeset: {e}")))?;

        let proposal_id = Uuid::new_v4();
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();

        // BUG-6 fix: validate parent_id exists in proposals_open before creating the
        // amendment proposal.  ADR-046 §2 says parent_id is set when amending an
        // earlier proposal after RequestChanges; an orphaned parent_id (pointing at
        // a non-existent proposal) corrupts the amendment chain.
        let validated_parent_id: Option<khive_types::Id128> = p
            .parent_id
            .as_deref()
            .map(|s| -> Result<khive_types::Id128, RuntimeError> {
                let parent_uuid = Uuid::from_str(s).map_err(|e| {
                    RuntimeError::InvalidInput(format!("invalid parent_id {s:?}: {e}"))
                })?;
                Ok(khive_types::Id128::from_u128(parent_uuid.as_u128()))
            })
            .transpose()?;

        if let Some(ref parent_id128) = validated_parent_id {
            let parent_uuid = Uuid::from_u128(parent_id128.to_u128());
            let sql = self.runtime.sql();
            let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
            let parent_row = reader
                .query_row(SqlStatement {
                    sql: "SELECT status FROM proposals_open \
                          WHERE proposal_id = ?1 AND namespace = ?2"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(parent_uuid.to_string()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: Some("proposals_open.validate_parent_id".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;
            if parent_row.is_none() {
                return Err(RuntimeError::InvalidInput(format!(
                    "parent_id {:?} not found; it must reference an existing proposal",
                    parent_uuid.to_string()
                )));
            }
        }

        let payload = ProposalCreatedPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            proposer: actor.clone(),
            title: p.title.clone(),
            description: p.description.clone(),
            changeset: _changeset,
            reviewers: p.reviewers.clone(),
            expiry: p
                .expiry
                .map(|v| khive_types::Timestamp::from_micros(v as u64)),
            parent_id: validated_parent_id,
        };

        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize proposal payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "propose",
            EventKind::ProposalCreated,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let event_store = self.runtime.events(token)?;
        event_store
            .append_event(event)
            .await
            .map_err(RuntimeError::Storage)?;

        // ADR-046 §4: projection is maintained by ProposalsProjectionWorker, not inline here.
        crate::projection_worker::ProposalsProjectionWorker::new(self.runtime.clone())
            .on_proposal_created(token, proposal_id, &actor, &p.title, p.expiry)
            .await?;

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "status": "open",
            "proposer": actor,
            "title": p.title,
        }))
    }

    /// `review` — declaration verb. Emits a `ProposalReviewed` event; side effects
    /// (projection update, changeset apply) are delegated to worker structs (ADR-046 §4-5).
    pub(crate) async fn handle_review(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: ReviewParams = deser(params)?;
        // H1: accept 8-char short IDs (propose returns short IDs; the next natural
        // step is review; forcing a full-UUID round-trip is unnecessary friction).
        let proposal_id = self.resolve_proposal_uuid(token, &p.proposal_id).await?;
        // Actor is always the authenticated token identity — client cannot override.
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();

        let decision: ProposalDecision = match p.decision.trim().to_ascii_lowercase().as_str() {
            "approve" => ProposalDecision::Approve,
            "reject" => ProposalDecision::Reject,
            "comment" => ProposalDecision::Comment,
            "request_changes" | "requestchanges" => ProposalDecision::RequestChanges,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown decision {other:?}; valid: approve | reject | comment | request_changes"
                )));
            }
        };

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposer, status FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("proposals_open.get".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?
            .ok_or_else(|| RuntimeError::NotFound(format!("proposal {}", p.proposal_id)))?;

        let proposer = row
            .get("proposer")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let current_status = row
            .get("status")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("open");

        // BUG-5 fix: 'approved' is a terminal state for the review(approve) path.
        // Without this guard a second review(approve) on an already-approved proposal
        // would silently succeed, inflating approve_count and creating spurious audit
        // events.  'approved' is included here alongside the other terminal states.
        // Per ADR-046 §4 the apply worker runs inline after approve and sets
        // status='applied'; after that point 'applied' also blocks re-review.
        if matches!(
            current_status,
            "applied" | "withdrawn" | "rejected" | "approved"
        ) {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already {current_status} and cannot be reviewed",
                p.proposal_id
            )));
        }

        // Self-approval guard: the proposer cannot approve their own proposal.
        // Exception: OSS local mode (`actor == "local"`) operates as a single-user
        // system where every operation runs under the same anonymous identity, so
        // the guard would unconditionally block all approvals. Skip it in that case.
        // Multi-actor deployments (where distinct actor IDs are assigned) enforce
        // the guard normally.
        if decision == ProposalDecision::Approve && actor == proposer && actor != "local" {
            return Err(RuntimeError::InvalidInput(format!(
                "self-approval is forbidden: proposer {actor:?} cannot approve their own proposal"
            )));
        }

        let payload = ProposalReviewedPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            reviewer: actor.clone(),
            decision,
            comment: p.comment.clone(),
        };
        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize review payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "review",
            EventKind::ProposalReviewed,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        // Compute response status for the ACK (mirrors what the projection worker writes).
        let new_status = match decision {
            ProposalDecision::Approve => "approved",
            ProposalDecision::Reject => "rejected",
            ProposalDecision::Comment => current_status,
            ProposalDecision::RequestChanges => "changes_requested",
        };

        // H2 fix (atomic CAS + event):
        // `reviewed_and_emit` runs the projection CAS UPDATE and the ProposalReviewed
        // event INSERT in a single `BEGIN IMMEDIATE` transaction.  This ensures the
        // event log and the projection always advance together — a process crash between
        // the two cannot leave a committed projection state without a corresponding event.
        //
        // If the CAS loses (concurrent op won the race), `reviewed_and_emit` returns
        // cas_hit=false.  In that case NEITHER the projection NOR the event was written
        // (because the batch transaction rolled back).  We return an error and the audit
        // log stays clean.
        let decision_changes_state = decision != ProposalDecision::Comment;
        let (projection_updated, _event_id) =
            crate::projection_worker::ProposalsProjectionWorker::new(self.runtime.clone())
                .reviewed_and_emit(token, &payload, event, decision_changes_state)
                .await?;

        if !projection_updated && decision_changes_state {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} status changed concurrently; review was not recorded — \
                 the proposal may have been withdrawn or approved by another reviewer \
                 simultaneously",
                p.proposal_id
            )));
        }

        // ADR-046 §5: apply worker fires on approval — idempotent on status check.
        if decision == ProposalDecision::Approve {
            crate::apply_worker::ProposalApplyWorker::new(self.runtime.clone())
                .maybe_apply(token, proposal_id, registry)
                .await?;
        }

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "reviewer": actor,
            "decision": p.decision,
            "status": new_status,
        }))
    }

    /// `withdraw` — commissive verb. Emits a `ProposalWithdrawn` event; projection
    /// is updated by ProposalsProjectionWorker (ADR-046 §4).
    pub(crate) async fn handle_withdraw(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: WithdrawParams = deser(params)?;
        // H1: accept 8-char short IDs consistent with all other verbs.
        let proposal_id = self.resolve_proposal_uuid(token, &p.proposal_id).await?;
        // Actor is always the authenticated token identity — client cannot override.
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposer, status FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("proposals_open.get_for_withdraw".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?
            .ok_or_else(|| RuntimeError::NotFound(format!("proposal {}", p.proposal_id)))?;

        let proposer = row
            .get("proposer")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if actor != proposer {
            return Err(RuntimeError::InvalidInput(format!(
                "only the original proposer {proposer:?} may withdraw this proposal"
            )));
        }

        let current_status = row
            .get("status")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("open");

        // H1 fix: 'applying' is a transient state owned by the apply worker — once
        // the apply worker claims it via pre_apply_cas, no withdraw can land.
        if matches!(current_status, "applied" | "withdrawn" | "applying") {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already {current_status} and cannot be withdrawn",
                p.proposal_id
            )));
        }

        let payload = ProposalWithdrawnPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            by: actor.clone(),
            reason: p.rationale.clone(),
        };
        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize withdraw payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "withdraw",
            EventKind::ProposalWithdrawn,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        // H2 fix (atomic CAS + event):
        // `withdrawn_and_emit` runs the projection CAS UPDATE and the ProposalWithdrawn
        // event INSERT in a single `BEGIN IMMEDIATE` transaction — projection and event
        // log always advance together.  If the CAS loses (concurrent op claimed
        // 'applying' or terminal state), NEITHER the projection NOR the event is
        // written; we return an error to the caller.
        let (updated, _event_id) =
            crate::projection_worker::ProposalsProjectionWorker::new(self.runtime.clone())
                .withdrawn_and_emit(token, proposal_id, event)
                .await?;

        if !updated {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already in a terminal or in-flight state and cannot be withdrawn",
                p.proposal_id
            )));
        }

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "status": "withdrawn",
            "by": actor,
        }))
    }

    /// `list(kind=proposal)` — assertive verb. Queries the `proposals_open`
    /// projection table with optional status / proposer filters.
    pub(crate) async fn handle_list_proposals(
        &self,
        token: &NamespaceToken,
        mut params: Value,
    ) -> Result<Value, RuntimeError> {
        // Strip the `kind` discriminator — ListProposalsParams uses
        // deny_unknown_fields and `kind` is the routing field, not a filter.
        if let Some(obj) = params.as_object_mut() {
            obj.remove("kind");
        }
        let p: ListProposalsParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))?;
        let ns = token.namespace().as_str().to_owned();
        let limit = p.limit.unwrap_or(50).min(500) as i64;
        let offset = p.offset.unwrap_or(0) as i64;

        let mut sql_str = "\
            SELECT proposal_id, proposer, title, status, created_at, updated_at, \
                   expiry, last_decision, review_count, approve_count, reject_count \
            FROM proposals_open \
            WHERE namespace = ?1"
            .to_string();
        let mut sql_params: Vec<SqlValue> = vec![SqlValue::Text(ns)];
        let mut param_idx = 2usize;

        // ADR-046:277-279 — hard-state proposals (approved/rejected/applied/withdrawn)
        // are retained for audit.  ADR-046:501-504 says list(kind=proposal) supports
        // standard filters.  When no status is supplied, return ALL rows so callers
        // can see the complete audit trail.  Pass status="open" (or repeat with
        // status="changes_requested") to filter to actionable proposals only.
        if let Some(status) = &p.status {
            sql_str.push_str(&format!(" AND status = ?{param_idx}"));
            sql_params.push(SqlValue::Text(status.clone()));
            param_idx += 1;
        }

        if let Some(proposer) = &p.proposer {
            sql_str.push_str(&format!(" AND proposer = ?{param_idx}"));
            sql_params.push(SqlValue::Text(proposer.clone()));
            param_idx += 1;
        }

        sql_str.push_str(&format!(
            " ORDER BY updated_at DESC LIMIT ?{param_idx} OFFSET ?{}",
            param_idx + 1
        ));
        sql_params.push(SqlValue::Integer(limit));
        sql_params.push(SqlValue::Integer(offset));

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let rows = reader
            .query_all(SqlStatement {
                sql: sql_str,
                params: sql_params,
                label: Some("proposals_open.list".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        let items: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let get_text = |name: &str| -> String {
                    row.get(name)
                        .and_then(|v| {
                            if let SqlValue::Text(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                };
                let get_int = |name: &str| -> Option<i64> {
                    row.get(name).and_then(|v| {
                        if let SqlValue::Integer(i) = v {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                };
                // ADR-045 §5: convert microsecond epoch integers to ISO-8601
                // strings before the MCP boundary (proposal listing fix).
                let ts_or_null = |name: &str| -> Value {
                    match get_int(name) {
                        Some(micros) => Value::String(micros_to_iso(micros)),
                        None => Value::Null,
                    }
                };
                serde_json::json!({
                    "proposal_id": get_text("proposal_id"),
                    "proposer": get_text("proposer"),
                    "title": get_text("title"),
                    "status": get_text("status"),
                    "created_at": ts_or_null("created_at"),
                    "updated_at": ts_or_null("updated_at"),
                    "expiry": ts_or_null("expiry"),
                    "last_decision": get_text("last_decision"),
                    "review_count": get_int("review_count").unwrap_or(0),
                    "approve_count": get_int("approve_count").unwrap_or(0),
                    "reject_count": get_int("reject_count").unwrap_or(0),
                })
            })
            .collect();

        to_json(&items)
    }

    pub(crate) async fn handle_stats(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _p: StatsParams = deser(params)?;
        let entities = self.runtime.count_entities(token, None).await?;
        let edges = self
            .runtime
            .count_edges(token, EdgeListFilter::default())
            .await?;
        let notes = self
            .runtime
            .notes(token)?
            .count_notes(token.namespace().as_str(), None)
            .await?;
        Ok(json!({
            "entities": entities,
            "edges": edges,
            "notes": notes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_relation, UpdateParams};
    use serde_json::json;

    // F009 (CRIT): error text must be derived from EdgeRelation::ALL, not a hardcoded list.
    // ADR-002 mandates 15 relations; error text must include derived_from and precedes.
    #[test]
    fn parse_relation_error_lists_all_relations() {
        let err = parse_relation("not_a_relation").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("derived_from"),
            "F009: parse_relation error must list derived_from (ADR-002); got: {msg}"
        );
        assert!(
            msg.contains("precedes"),
            "F009: parse_relation error must list precedes (ADR-002); got: {msg}"
        );
    }

    // ADR-014: wire-level tri-state nullable f64 for `update`.
    //   absent  → outer None (preserve existing value)
    //   null    → Some(None) (clear the value)
    //   number  → Some(Some(v)) (set to v)
    //
    // Regression for round-3 finding: the previous `Option<Value>` representation
    // collapsed absent and null into the same `None`, so JSON null could not
    // distinguish "clear" from "preserve" through the MCP wire surface.
    #[test]
    fn update_params_tri_state_salience() {
        let absent: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note"})).unwrap();
        assert_eq!(
            absent.salience, None,
            "absent salience key must deserialize to outer None (preserve)"
        );

        let cleared: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "salience": null})).unwrap();
        assert_eq!(
            cleared.salience,
            Some(None),
            "salience=null must deserialize to Some(None) (clear)"
        );

        let set: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "salience": 0.5})).unwrap();
        assert_eq!(
            set.salience,
            Some(Some(0.5)),
            "salience=0.5 must deserialize to Some(Some(0.5)) (set)"
        );
    }

    #[test]
    fn update_params_tri_state_decay_factor() {
        let absent: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note"})).unwrap();
        assert_eq!(
            absent.decay_factor, None,
            "absent decay_factor key must deserialize to outer None (preserve)"
        );

        let cleared: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "decay_factor": null}))
                .unwrap();
        assert_eq!(
            cleared.decay_factor,
            Some(None),
            "decay_factor=null must deserialize to Some(None) (clear)"
        );

        let set: UpdateParams =
            serde_json::from_value(json!({"id": "x", "kind": "note", "decay_factor": 0.6}))
                .unwrap();
        assert_eq!(
            set.decay_factor,
            Some(Some(0.6)),
            "decay_factor=0.6 must deserialize to Some(Some(0.6)) (set)"
        );
    }

    // ADR-046: resolve_kind_spec must recognise "proposal" as KindSpec::Proposal
    #[test]
    fn resolve_kind_spec_proposal() {
        use super::{resolve_kind_spec, KindSpec};
        use crate::KgPack;
        use khive_runtime::VerbRegistryBuilder;

        let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        let registry = builder.build().expect("registry build");

        let spec = resolve_kind_spec("proposal", &registry).expect("should resolve proposal");
        assert_eq!(
            spec,
            KindSpec::Proposal,
            "kind=proposal must resolve to KindSpec::Proposal"
        );

        let spec_upper =
            resolve_kind_spec("Proposal", &registry).expect("should be case-insensitive");
        assert_eq!(
            spec_upper,
            KindSpec::Proposal,
            "kind=Proposal (mixed case) must resolve"
        );
    }

    // ADR-046: propose param deserialization
    #[test]
    fn propose_params_deserialization() {
        use super::ProposeParams;
        let p: ProposeParams = serde_json::from_value(json!({
            "title": "Add RoPE",
            "description": "Add RoPE entity to the graph",
            "changeset": {
                "kind": "add_entity",
                "entity": {"kind": "concept", "name": "RoPE"}
            },
            "reviewers": ["alice"],
        }))
        .expect("ProposeParams must deserialize");
        assert_eq!(p.title, "Add RoPE");
        assert_eq!(p.reviewers, vec!["alice"]);
        assert!(p.parent_id.is_none());
        assert!(p.expiry.is_none());
    }

    // ADR-046: review param deserialization with all valid decisions
    #[test]
    fn review_params_decisions() {
        use super::ReviewParams;
        for decision in ["approve", "reject", "comment", "request_changes"] {
            let p: ReviewParams = serde_json::from_value(json!({
                "proposal_id": "00000000-0000-0000-0000-000000000001",
                "decision": decision,
            }))
            .expect("ReviewParams must deserialize");
            assert_eq!(p.decision, decision);
        }
    }

    // CRIT-2 regression: ReviewParams must not accept an `actor` field.
    // The actor is always derived from the NamespaceToken at dispatch time.
    // If a client passes actor=<other_id>, the field is ignored (unknown fields
    // are allowed by serde default, so the struct simply lacks the field).
    #[test]
    fn review_params_no_actor_field() {
        use super::ReviewParams;
        // Baseline: ReviewParams works without actor.
        let p: ReviewParams = serde_json::from_value(json!({
            "proposal_id": "00000000-0000-0000-0000-000000000001",
            "decision": "approve",
        }))
        .expect("ReviewParams must deserialize without actor");
        assert_eq!(p.proposal_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!(p.decision, "approve");
    }

    // CRIT-2 regression: WithdrawParams must not accept an `actor` field.
    #[test]
    fn withdraw_params_no_actor_field() {
        use super::WithdrawParams;
        let p: WithdrawParams = serde_json::from_value(json!({
            "proposal_id": "00000000-0000-0000-0000-000000000002",
        }))
        .expect("WithdrawParams must deserialize without actor");
        assert_eq!(p.proposal_id, "00000000-0000-0000-0000-000000000002");
        assert!(p.rationale.is_none());
    }

    // CRIT-2 regression: ProposeParams must not accept an `actor` field.
    #[test]
    fn propose_params_no_actor_field() {
        use super::ProposeParams;
        let p: ProposeParams = serde_json::from_value(json!({
            "title": "Fix RoPE",
            "description": "Fix RoPE entity",
            "changeset": {"kind": "add_entity", "entity": {"kind": "concept", "name": "RoPE"}},
        }))
        .expect("ProposeParams must deserialize without actor");
        assert_eq!(p.title, "Fix RoPE");
    }

    // ADR-046: KG pack must expose exactly 14 handlers including propose/review/withdraw
    #[test]
    fn kg_pack_exposes_16_handlers() {
        use crate::KgPack;
        use khive_types::Pack;
        let handlers = KgPack::HANDLERS;
        assert_eq!(
            handlers.len(),
            16,
            "kg pack must expose 16 handlers (was 15, +1 for stats — #280)"
        );
        let names: Vec<&str> = handlers.iter().map(|h| h.name).collect();
        assert!(names.contains(&"propose"), "propose must be in KG_HANDLERS");
        assert!(names.contains(&"review"), "review must be in KG_HANDLERS");
        assert!(
            names.contains(&"withdraw"),
            "withdraw must be in KG_HANDLERS"
        );
        assert!(names.contains(&"verbs"), "verbs must be in KG_HANDLERS");
        assert!(names.contains(&"stats"), "stats must be in KG_HANDLERS");
    }

    // ---- Wave 4 regression tests ----

    // CC-2 regression: ListParams must accept a `tags` field.
    // Before the fix, tags was absent from the struct so passing tags=["rust"]
    // silently discarded the filter.
    #[test]
    fn list_params_accepts_tags() {
        use super::ListParams;
        let p: ListParams = serde_json::from_value(json!({
            "kind": "entity",
            "tags": ["rust", "systems"],
        }))
        .expect("ListParams must accept tags");
        assert_eq!(
            p.tags,
            Some(vec!["rust".to_string(), "systems".to_string()])
        );
    }

    // CC-2 regression: ListParams with no tags field produces None (not empty vec).
    #[test]
    fn list_params_no_tags_is_none() {
        use super::ListParams;
        let p: ListParams = serde_json::from_value(json!({"kind": "entity"})).unwrap();
        assert!(
            p.tags.is_none(),
            "absent tags must be None so the entity filter is not applied"
        );
    }

    // ue-kg-deep C3 regression: UpdateParams must capture entity_kind so the
    // handler can return an explicit error instead of silently discarding it.
    #[test]
    fn update_params_captures_entity_kind() {
        use super::UpdateParams;
        let p: UpdateParams = serde_json::from_value(json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "entity_kind": "dataset",
        }))
        .expect("UpdateParams must deserialize with entity_kind present");
        assert!(
            p.entity_kind.is_some(),
            "entity_kind field must be captured (not silently discarded)"
        );
    }

    // ue-kg-deep C3 regression: absent entity_kind → None (preserves normal update flow).
    #[test]
    fn update_params_entity_kind_absent_is_none() {
        use super::UpdateParams;
        let p: UpdateParams = serde_json::from_value(json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "NewName",
        }))
        .unwrap();
        assert!(
            p.entity_kind.is_none(),
            "absent entity_kind must be None so normal updates are not rejected"
        );
    }

    // ue-kg-deep C4 regression: SearchParams must accept a `min_score` field.
    #[test]
    fn search_params_accepts_min_score() {
        use super::SearchParams;
        let p: SearchParams = serde_json::from_value(json!({
            "kind": "entity",
            "query": "transformer",
            "min_score": 0.1,
        }))
        .expect("SearchParams must accept min_score");
        assert_eq!(p.min_score, Some(0.1));
    }

    // ue-kg-deep C4 regression: absent min_score → None (no floor applied, returns all hits).
    #[test]
    fn search_params_min_score_absent_is_none() {
        use super::SearchParams;
        let p: SearchParams = serde_json::from_value(json!({
            "kind": "entity",
            "query": "transformer",
        }))
        .unwrap();
        assert!(
            p.min_score.is_none(),
            "absent min_score must be None; no floor applied by default"
        );
    }

    // ADR-045 §5 C1: entity timestamps must be ISO-8601 strings at the handler
    // boundary. Entity.created_at / updated_at are stored as i64 microseconds;
    // normalize_entity_timestamps converts them before MCP serialization.

    // ---- Round-6: recursive walk_timestamps unit tests ----

    #[test]
    fn walk_timestamps_converts_top_level_created_at() {
        use super::walk_timestamps;
        let micros = 1779757074693195i64;
        let mut v = json!({ "created_at": micros, "name": "test" });
        walk_timestamps(&mut v);
        let s = v["created_at"].as_str().expect("must be string");
        assert!(s.len() >= 20 && s.contains('T'), "must be ISO-8601: {s}");
        assert_eq!(v["name"], json!("test"), "name must be unchanged");
    }

    #[test]
    fn walk_timestamps_converts_nested_object_timestamp() {
        use super::walk_timestamps;
        let micros = 1_779_757_074_693_195u64;
        let mut v = json!({
            "payload": {
                "result": { "applied_at": micros }
            }
        });
        walk_timestamps(&mut v);
        let s = v["payload"]["result"]["applied_at"]
            .as_str()
            .expect("payload.result.applied_at must be string");
        assert!(s.len() >= 20 && s.contains('T'), "must be ISO-8601: {s}");
    }

    #[test]
    fn walk_timestamps_converts_array_element_timestamps() {
        use super::walk_timestamps;
        let micros1 = 1_779_757_074_000_000u64;
        let micros2 = 1_779_757_075_000_000u64;
        let mut v = json!({
            "payload": {
                "steps": [
                    { "updated_at": micros1 },
                    { "updated_at": micros2 }
                ]
            }
        });
        walk_timestamps(&mut v);
        let steps = v["payload"]["steps"].as_array().unwrap();
        for step in steps {
            let s = step["updated_at"]
                .as_str()
                .expect("array element updated_at must be string");
            assert!(s.len() >= 20 && s.contains('T'), "must be ISO-8601: {s}");
        }
    }

    #[test]
    fn walk_timestamps_handles_i64_branch() {
        use super::walk_timestamps;
        // i64 — covers legacy fields and the as_i64() branch of the conversion.
        let micros: i64 = 1_234_567_890_000_000;
        let mut v = json!({ "applied_at": micros });
        walk_timestamps(&mut v);
        let s = v["applied_at"].as_str().expect("must be string");
        assert!(s.contains('T'), "must be ISO-8601: {s}");
    }

    #[test]
    fn walk_timestamps_leaves_strings_unchanged() {
        use super::walk_timestamps;
        let iso = "2026-05-26T00:00:00+00:00";
        let mut v = json!({ "created_at": iso });
        walk_timestamps(&mut v);
        assert_eq!(v["created_at"].as_str().unwrap(), iso);
    }

    #[test]
    fn walk_timestamps_leaves_null_unchanged() {
        use super::walk_timestamps;
        let mut v = json!({ "deleted_at": null, "created_at": 1779757074693195i64 });
        walk_timestamps(&mut v);
        assert_eq!(v["deleted_at"], json!(null));
        assert!(
            v["created_at"].as_str().is_some(),
            "created_at must be converted"
        );
    }

    #[test]
    fn walk_timestamps_non_timestamp_number_untouched() {
        use super::walk_timestamps;
        // A key that is NOT in TIMESTAMP_KEYS — must not be touched.
        let mut v = json!({ "count": 42, "created_at": 1779757074693195i64 });
        walk_timestamps(&mut v);
        assert_eq!(
            v["count"],
            json!(42),
            "non-timestamp number must be unchanged"
        );
        assert!(v["created_at"].as_str().is_some());
    }

    #[test]
    fn normalize_entity_timestamps_converts_i64_to_iso() {
        use super::normalize_entity_timestamps;
        // 2026-05-26T00:57:54.693195Z → micros since epoch
        let micros = 1779757074693195i64;
        let v = json!({ "created_at": micros, "updated_at": micros, "name": "test" });
        let out = normalize_entity_timestamps(v);
        let created = out["created_at"]
            .as_str()
            .expect("created_at must be a string");
        let updated = out["updated_at"]
            .as_str()
            .expect("updated_at must be a string");
        // Both must look like ISO-8601 (start with 4-digit year, contain 'T').
        assert!(
            created.len() >= 20 && created.contains('T'),
            "created_at must be ISO-8601, got: {created:?}"
        );
        assert!(
            updated.len() >= 20 && updated.contains('T'),
            "updated_at must be ISO-8601, got: {updated:?}"
        );
        // name must be unchanged.
        assert_eq!(out["name"], json!("test"));
    }

    #[test]
    fn normalize_entity_timestamps_leaves_string_unchanged() {
        use super::normalize_entity_timestamps;
        let iso = "2026-05-26T00:57:54.693195+00:00";
        let v = json!({ "created_at": iso, "updated_at": iso });
        let out = normalize_entity_timestamps(v);
        // Already a string — must not be double-converted.
        assert_eq!(out["created_at"].as_str().unwrap(), iso);
    }

    #[test]
    fn normalize_entity_timestamps_leaves_null_unchanged() {
        use super::normalize_entity_timestamps;
        let v = json!({ "created_at": 1779757074693195i64, "deleted_at": null });
        let out = normalize_entity_timestamps(v);
        assert!(
            out["created_at"].as_str().is_some(),
            "created_at must be converted"
        );
        assert_eq!(out["deleted_at"], json!(null), "null must remain null");
    }

    #[test]
    fn normalize_entity_timestamps_array_converts_each_element() {
        use super::normalize_entity_timestamps_array;
        let micros = 1779757074693195i64;
        let v = json!([
            { "created_at": micros, "name": "a" },
            { "created_at": micros, "name": "b" },
        ]);
        let out = normalize_entity_timestamps_array(v);
        let arr = out.as_array().unwrap();
        for item in arr {
            assert!(
                item["created_at"].as_str().is_some(),
                "each element's created_at must be ISO string"
            );
        }
    }

    // ---- Issue #486: link endpoint validation should suggest valid relations ----

    // Unit test: valid_relations_for_entity_pair returns expected relations for known pairs.
    #[test]
    fn valid_relations_concept_to_concept_includes_extends() {
        use super::valid_relations_for_entity_pair;
        let rels = valid_relations_for_entity_pair("concept", "concept");
        assert!(
            rels.contains(&"extends"),
            "#486: concept->concept must include extends; got: {rels:?}"
        );
        assert!(
            rels.contains(&"competes_with"),
            "#486: concept->concept must include competes_with; got: {rels:?}"
        );
        assert!(
            rels.contains(&"composed_with"),
            "#486: concept->concept must include composed_with; got: {rels:?}"
        );
        assert!(
            rels.contains(&"instance_of"),
            "#486: concept->concept must include instance_of (wildcard src); got: {rels:?}"
        );
    }

    // Unit test: unsupported endpoint pair returns empty vec (not a panic).
    #[test]
    fn valid_relations_unsupported_pair_returns_empty() {
        use super::valid_relations_for_entity_pair;
        let rels = valid_relations_for_entity_pair("person", "dataset");
        assert!(
            rels.is_empty(),
            "#486: person->dataset has no base-contract relations; got: {rels:?}"
        );
    }

    // Integration test: link with invalid relation returns error containing valid relations.
    #[tokio::test]
    async fn link_invalid_relation_error_suggests_valid_relations() {
        use crate::KgPack;
        use khive_runtime::KhiveRuntime;

        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

        let src_val = rt
            .create_entity(&token, "concept", None, "ConceptA", None, None, vec![])
            .await
            .expect("create source entity");
        let tgt_val = rt
            .create_entity(&token, "concept", None, "ConceptB", None, None, vec![])
            .await
            .expect("create target entity");

        let pack = KgPack::new(rt.clone());

        // "depends_on" is a valid relation string but NOT in the concept->concept allowlist.
        let params = json!({
            "source_id": src_val.id.to_string(),
            "target_id": tgt_val.id.to_string(),
            "relation": "depends_on",
        });
        let result = pack.handle_link(&token, params).await;
        assert!(
            result.is_err(),
            "#486: depends_on on concept->concept should fail"
        );
        let err_msg = format!("{}", result.unwrap_err());
        // The enriched error must mention valid relations.
        assert!(
            err_msg.contains("Valid relations:"),
            "#486: error must contain 'Valid relations:'; got: {err_msg}"
        );
        // concept->concept includes extends; verify it appears in the suggestion.
        assert!(
            err_msg.contains("extends"),
            "#486: valid relations for concept->concept must include 'extends'; got: {err_msg}"
        );
        // The error should name the endpoint kinds.
        assert!(
            err_msg.contains("concept"),
            "#486: error must mention endpoint kinds; got: {err_msg}"
        );
    }
}
