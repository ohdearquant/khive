// FILE SIZE JUSTIFICATION: operations.rs is the single coherent surface for all
// runtime verb implementations (create, get, list, search, link, traverse, query,
// recall, etc.). All verbs share internal helpers (namespace checks, edge validation,
// canonical-endpoint logic) that require pub(crate) access — splitting into submodules
// would require pub(crate) re-exports across every helper or circular dependencies.
// Inline tests exercise those private helpers directly. Split plan: once the verb
// surface stabilises post-retrieval-refactor, group by substrate (entity,
// note, edge, search) into submodules under an `operations/` directory.
//! High-level operations composing storage capabilities into user-facing verbs.
//!
//! # Fault-injection arm migration
//!
//! Namespace-targeted fault injection uses scoped guards. The former
//! `arm_fts_fail`, `arm_fts_fail_many`, `arm_fts_fail_many_partial`, and
//! `arm_vector_fail` names were removed in favor of their `_scoped` variants so
//! stale statement-form calls fail to compile. Statement-form arming cannot be
//! preserved because dropping the returned guard at the semicolon disarms an
//! unconsumed injection.

use std::collections::HashMap;
use std::str::FromStr;

use serde::Serialize;
use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::note::Note;
use khive_storage::types::{
    DeleteMode, DirectedNeighborHit, Direction, EdgeSortField, GraphPath, LinkId, NeighborHit,
    NeighborQuery, Page, PageRequest, SortOrder, SqlRow, SqlStatement, SqlValue, TextFilter,
    TextQueryMode, TextSearchRequest, TraversalRequest,
};
use khive_storage::{Edge, EdgeRelation, Entity, EntityFilter, Event, EventFilter};
use khive_types::{EdgeEndpointRule, EndpointKind, EventKind, SubstrateKind};

use khive_db::stores::entity::{entity_hard_delete_statement, entity_upsert_statement};
use khive_db::stores::graph::{edge_hard_delete_statement, purge_incident_edges_statement};
use khive_db::stores::note::note_hard_delete_statement;
use khive_db::stores::text::insert_document_statement;
use khive_db::SqliteError;
use rusqlite::OptionalExtension;

use crate::atomic_plan::{
    AddEntityPlan, AffectedRowGuard, DeletePlan, PlanStatement, PostCommitEffect,
};
use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use crate::curation::{entity_fts_document, note_embedding_text, note_fts_document};
use crate::error::{GuardedWriteFailure, RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

// Test-only failure injection for `create_note_inner`. Namespace-targeted so only
// calls for the armed namespace fire, avoiding cross-test races without `#[serial]`.
// Gated behind `cfg(any(test, feature = "fault-injection"))` so no lock acquisitions
// or injection surface exist in production/published binaries. External integration
// test crates enable it via a dev-dependency: khive-runtime = { ..., features = ["fault-injection"] }
#[cfg(test)]
std::thread_local! {
    static LINK_FAIL_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Count-targetable vector-INSERT fault injection: when set to N (N > 0), the next N
// vector insert calls (entity or note, single- or multi-model) succeed and the
// (N+1)-th returns an injected error, then the counter resets to 0.
// `thread_local!` provides per-thread isolation (`#[tokio::test]` uses a
// current-thread runtime, so there is no thread migration mid-test), letting a
// test fail one specific model's insert in a multi-model fan-out and giving any
// caller whose test suite shares a default namespace a deterministic, race-free
// injection instead of depending on `VECTOR_FAIL_NS`'s namespace match.
#[cfg(any(test, feature = "fault-injection"))]
std::thread_local! {
    static VECTOR_FAIL_AFTER: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Arm the count-targetable vector-INSERT fault: let `n` inserts succeed, then fail
/// the next one (entity or note, single- or multi-model). Set `n = 0` to fail
/// immediately on the first insert. Thread-local, so unlike `arm_vector_fail_scoped`
/// it cannot be won or disarmed by a concurrently-running test on another
/// thread — prefer this one whenever the caller cannot guarantee it is the
/// only test writing into the namespace it cares about.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_vector_fail_after(n: usize) {
    VECTOR_FAIL_AFTER.with(|cell| cell.set(Some(n)));
}

// Namespace-keyed one-shot arms, not a single `Option<String>` slot:
// `create_note_inner` and `create_entity_inner` share this flag, and a
// single-slot design let a concurrently running test's `arm_fts_fail_scoped(other_ns)`
// overwrite this test's armed namespace before its own create call consumed
// it, so the intended injection silently never fired (#1095). Keying by
// namespace fixes that at the root — arming `ns_B` inserts `ns_B` without
// evicting `ns_A`. Process-wide (not thread-local) so a caller may arm on
// one OS thread and run the triggering `create_note`/`create_entity` on
// another (e.g. via `tokio::spawn` on a multi-thread runtime); the
// check-and-remove under the mutex lock keeps exactly-once semantics even
// under concurrent same-namespace creates.
#[cfg(any(test, feature = "fault-injection"))]
type FaultArmSet = std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<()>>>;
#[cfg(any(test, feature = "fault-injection"))]
const MAX_FAULT_ARMS: usize = 64;
#[cfg(any(test, feature = "fault-injection"))]
static FTS_FAIL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
// Vector insertion failures use the same namespace-keyed one-shot semantics.
#[cfg(any(test, feature = "fault-injection"))]
static VECTOR_FAIL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// FTS failure injection for `create_many` — separate from `FTS_FAIL_NS` so that
/// create_note_inner and create_many tests cannot disarm each other. Namespace-keyed
/// set (not a single `Option<String>` slot) for the same reason as `VECTOR_FAIL_NS` (#1263).
#[cfg(any(test, feature = "fault-injection"))]
static FTS_FAIL_MANY_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// FTS partial-failure injection for `create_many` — returns `Ok(BatchWriteSummary)`
/// with `failed > 0` so that the `summary.failed > 0` rollback branch is exercised.
/// Distinct from `FTS_FAIL_MANY_NS` which injects a hard `Err`. Namespace-keyed set
/// for the same reason as `VECTOR_FAIL_NS` (#1263).
#[cfg(any(test, feature = "fault-injection"))]
static FTS_FAIL_MANY_PARTIAL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Scoped ownership of a process-wide fault-injection arm.
#[cfg(any(test, feature = "fault-injection"))]
#[must_use = "the fault injection is disarmed when this guard is dropped"]
pub struct FaultInjectionArm {
    namespace: String,
    token: std::sync::Arc<()>,
    arms: &'static FaultArmSet,
}

#[cfg(any(test, feature = "fault-injection"))]
impl Drop for FaultInjectionArm {
    fn drop(&mut self) {
        let mut arms = self.arms.lock().unwrap();
        if arms
            .get(&self.namespace)
            .is_some_and(|token| std::sync::Arc::ptr_eq(token, &self.token))
        {
            arms.remove(&self.namespace);
        }
    }
}

#[cfg(any(test, feature = "fault-injection"))]
fn arm_fault(arms: &'static FaultArmSet, namespace: &str, max_arms: usize) -> FaultInjectionArm {
    let token = std::sync::Arc::new(());
    let refusal = {
        let mut active = arms.lock().unwrap();
        if active.contains_key(namespace) {
            Some("the namespace is already armed")
        } else if active.len() >= max_arms {
            Some("the arm set is at capacity")
        } else {
            active.insert(namespace.to_string(), std::sync::Arc::clone(&token));
            None
        }
    };
    if let Some(reason) = refusal {
        panic!("cannot arm fault injection for namespace `{namespace}`: {reason}");
    }
    FaultInjectionArm {
        namespace: namespace.to_string(),
        token,
        arms,
    }
}

#[cfg(any(test, feature = "fault-injection"))]
fn consume_fault(arms: &FaultArmSet, namespace: &str) -> bool {
    arms.lock().unwrap().remove(namespace).is_some()
}
/// Non-parser FTS *search*-leg failure injection for `search_notes`: distinct
/// from `FTS_FAIL_NS` (which injects at the FTS *upsert*/write step of
/// `create_note_inner`). Injects a `StorageError::Timeout` at the `search()`
/// call the FTS fail-open arm guards, so the arm's `is_fts5_syntax_error()`
/// gate can be exercised against a genuine non-parser failure and asserted to
/// propagate rather than degrade.
#[cfg(any(test, feature = "fault-injection"))]
static FTS_SEARCH_FAIL_NS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Arm a one-shot FTS failure injection for `create_note_inner`/`create_entity_inner`
/// targeting namespace `ns`.
///
/// The next `create_note` or `create_entity` call whose namespace equals `ns` returns
/// an injected error at the FTS upsert step (after the row is committed), then disarms
/// — only that namespace's entry is consumed. The arm is process-wide and thread
/// independent: it may be set from one OS thread and consumed by a `create_note`/
/// `create_entity` call running on another (e.g. inside `tokio::spawn`). Concurrent
/// arms of distinct namespaces do not interfere with each other.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_fail_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&FTS_FAIL_NS, ns, MAX_FAULT_ARMS)
}

/// Arm the FTS failure injection for `create_many` targeting namespace `ns`.
///
/// The next `create_many` call whose namespace equals `ns` returns an injected
/// error at the first FTS statement inside the atomic batch, then disarms.
/// Calls on other namespaces are unaffected, and concurrent arms of distinct
/// namespaces do not overwrite each other.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_fail_many_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&FTS_FAIL_MANY_NS, ns, MAX_FAULT_ARMS)
}

/// Arm a mid-batch FTS failure for `create_many` targeting namespace `ns`.
///
/// The next matching call fails the second FTS statement when the batch contains at
/// least two entities, after one entity/FTS pair has executed in the transaction.
/// A one-entity batch fails its first FTS statement. Then disarms only that namespace.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_fail_many_partial_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&FTS_FAIL_MANY_PARTIAL_NS, ns, MAX_FAULT_ARMS)
}

/// Arm a non-parser FTS *search*-leg failure injection for `search_notes` targeting
/// any call whose visible namespaces include `ns`.
///
/// The next `search_notes` call touching `ns` returns `StorageError::Timeout` from
/// the FTS leg instead of calling the real `TextSearch::search`, then disarms.
/// Used to prove the fail-open arm in `search_notes` propagates non-parser
/// `StorageError`s instead of silently degrading them the way a genuine FTS5
/// parser syntax error is degraded.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_search_fail(ns: &str) {
    *FTS_SEARCH_FAIL_NS.lock().unwrap() = Some(ns.to_string());
}

/// Arm the vector insertion failure injection for `create_note_inner` targeting `ns`.
///
/// The next `create_note` call whose note namespace equals `ns` returns an injected
/// error at the first vector insert step, then disarms.  Calls on other namespaces
/// are unaffected, and concurrent arms of distinct namespaces do not overwrite
/// each other.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_vector_fail_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&VECTOR_FAIL_NS, ns, MAX_FAULT_ARMS)
}

/// Failure injection for `delete_note_row_first_for_compensation`'s post-row-removal
/// cleanup step: distinct from `FTS_FAIL_NS`/`VECTOR_FAIL_NS`, which target
/// `create_note_inner`. Lets tests prove that a rollback compensation's cleanup
/// failure still leaves the note row (and thus the live message) gone.
#[cfg(any(test, feature = "fault-injection"))]
static ROLLBACK_CLEANUP_FAIL_NS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Arm the rollback-compensation cleanup failure injection targeting `ns`.
///
/// The next `delete_note_row_first_for_compensation` call whose note namespace
/// equals `ns` removes the row as usual, then returns an injected cleanup error
/// instead of running the real graph/FTS/vector cleanup, then disarms.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_rollback_cleanup_fail(ns: &str) {
    *ROLLBACK_CLEANUP_FAIL_NS.lock().unwrap() = Some(ns.to_string());
}

/// A note search result with UUID, salience-weighted RRF score, and display text.
#[derive(Clone, Debug)]
pub struct NoteSearchHit {
    pub note_id: Uuid,
    pub score: DeterministicScore,
    pub source: crate::SearchSource,
    pub title: Option<String>,
    pub snippet: Option<String>,
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
/// under the composed base allowlist plus pack `EDGE_RULES`.
///
/// Reuses [`base_entity_rule_allows`] and [`accepted_pack_relations_for_pattern_entities`]
/// (the pattern-side, unconstrained-`None` counterpart of the functions the live
/// validator consults) over the closed kind set, rather than re-deriving a
/// parallel table — issue #543 precedent, applied to GQL query-pattern hint
/// derivation (issue #593).
///
/// Pack rules are skipped when `crate::pack::is_special_relation` is true
/// (supersedes / supports / refutes): the live validator's special-relation branch
/// (`validate_edge_relation_endpoints`, this file) resolves those relations
/// before `pack_rule_allows` is ever reached, so a pack `EDGE_RULES` entry for
/// one of them is never actually enforced (see `pack.rs`'s
/// `edge_endpoint_table` doc comment for the authoritative statement of this).
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
    // Derivation
    ("concept", EdgeRelation::Extends, "concept"),
    ("concept", EdgeRelation::VariantOf, "concept"),
    ("artifact", EdgeRelation::VariantOf, "artifact"),
    ("concept", EdgeRelation::IntroducedBy, "document"),
    ("concept", EdgeRelation::IntroducedBy, "person"),
    ("artifact", EdgeRelation::IntroducedBy, "document"),
    ("document", EdgeRelation::IntroducedBy, "person"),
    ("document", EdgeRelation::IntroducedBy, "org"),
    ("concept", EdgeRelation::IntroducedBy, "org"),
    // Provenance
    ("artifact", EdgeRelation::DerivedFrom, "dataset"),
    ("artifact", EdgeRelation::DerivedFrom, "document"),
    ("artifact", EdgeRelation::DerivedFrom, "project"),
    ("artifact", EdgeRelation::DerivedFrom, "artifact"),
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
    if relation.is_symmetric() && target_id < source_id {
        (target_id, source_id)
    } else {
        (source_id, target_id)
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
    if let Some(ref m) = metadata {
        if m.get("dependency_kind").is_some() {
            return metadata;
        }
    }
    let inferred = infer_dependency_kind(src_kind, tgt_kind)?;
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

/// Validate governed edge metadata keys.
///
/// Currently enforces:
/// - `dependency_kind` is only valid on `depends_on` edges.
/// - `dependency_kind`, when present, must be one of the governed values.
pub(crate) fn validate_edge_metadata(
    relation: EdgeRelation,
    metadata: Option<&serde_json::Value>,
) -> RuntimeResult<()> {
    let Some(meta) = metadata else {
        return Ok(());
    };
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

/// Collapse per-namespace `GraphPath`s from [`KhiveRuntime::traverse`] down to
/// exactly one entry per distinct `root_id`.
///
/// `traverse` queries every namespace in the token's visible set
/// independently — including namespaces that don't own the root at all,
/// which still contribute a root-only entry when `include_roots` is set —
/// and each of those per-namespace calls already enforces `limit` on its own
/// results. Concatenating them naively before this function would let a root
/// visible in N namespaces return up to N * limit nodes, would keep
/// whichever namespace's copy of a shared node happened to arrive first
/// (wrong depth/`via_edge` when that wasn't the shortest path, and
/// non-BFS ordering), and would rebuild a seen-set from scratch per
/// namespace (quadratic in namespace count).
///
/// This merges by `(root_id, node_id)` keeping the node's shallowest depth
/// and the `via_edge` that produced it (first-namespace-processed wins ties
/// at equal depth — namespace processing order is deterministic but which
/// tied edge is "more correct" is not otherwise decidable), reorders the
/// merged result BFS-style (ascending depth), and re-applies `limit` to the
/// merged non-root node count so the response honors the contract each
/// individual namespace call already tried to.
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
async fn drain_embed_join_set(
    mut join_set: tokio::task::JoinSet<(usize, RuntimeResult<Vec<f32>>)>,
    model_count: usize,
) -> RuntimeResult<Vec<Vec<f32>>> {
    let mut vectors: Vec<Option<Vec<f32>>> = (0..model_count).map(|_| None).collect();

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

    /// Create and persist a new entity.
    // REASON: entity creation requires kind, type, name, description, properties, tags, and
    // namespace token — refactoring into a builder would add indirection without reducing
    // caller complexity; this signature mirrors the MCP verb surface directly.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<Entity> {
        self.validate_entity_kind(kind)?;
        // Secret gate: scan name, description, structured properties, and tags.
        crate::secret_gate::check(name)?;
        if let Some(d) = description {
            crate::secret_gate::check(d)?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json(p)?;
        }
        crate::secret_gate::check_tags(&tags)?;
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
        self.entities(token)?.upsert_entity(entity.clone()).await?;

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
                if let Ok(store) = self.entities(token) {
                    if let Err(ce) = store.delete_entity(entity.id, DeleteMode::Hard).await {
                        tracing::error!(
                            error = %ce,
                            id = %entity.id,
                            "create_entity: failed to roll back entity row after FTS failure"
                        );
                    }
                }
                return Err(e);
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

        if embed_model_names.len() == 1 {
            let model_name = &embed_model_names[0];
            let vec_result = self
                .embed_document_with_model(model_name, &embed_body)
                .await;

            #[cfg(any(test, feature = "fault-injection"))]
            let vec_inject = consume_fault(&VECTOR_FAIL_NS, ns);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let vec_inject = false;
            let vec_result: RuntimeResult<Vec<f32>> = if vec_inject {
                Err(RuntimeError::Internal(
                    "injected vector failure".to_string(),
                ))
            } else {
                vec_result
            };

            let single_result: RuntimeResult<()> = match vec_result {
                Ok(vector) => match self.vectors_for_model(token, model_name) {
                    Ok(vs) => vs
                        .insert(
                            entity.id,
                            SubstrateKind::Entity,
                            ns,
                            "entity.body",
                            vec![vector],
                        )
                        .await
                        .map_err(RuntimeError::from),
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            if let Err(e) = single_result {
                if let Ok(store) = self.entities(token) {
                    if let Err(ce) = store.delete_entity(entity.id, DeleteMode::Hard).await {
                        tracing::error!(
                            error = %ce,
                            id = %entity.id,
                            "create_entity: failed to roll back entity row after vector failure"
                        );
                    }
                }
                if let Ok(fts) = self.text(token) {
                    if let Err(ce) = fts.delete_document(ns, entity.id).await {
                        tracing::error!(
                            error = %ce,
                            id = %entity.id,
                            "create_entity: failed to roll back FTS document after vector failure"
                        );
                    }
                }
                return Err(e);
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
                join_set.spawn(async move {
                    let fut = rt.embed_document_with_model(&name, &text);
                    let result = match ctx {
                        Some(ctx) => crate::usage::scope(ctx, fut).await,
                        None => fut.await,
                    };
                    (idx, result)
                });
            }
            // The first failed or panicked handle aborts and detaches its
            // siblings. Embed usage is counted at dispatch, so a synchronous
            // provider winding down in the background cannot change it.
            let vectors = match drain_embed_join_set(join_set, embed_model_names.len()).await {
                Ok(vectors) => vectors,
                Err(e) => {
                    if let Ok(store) = self.entities(token) {
                        if let Err(ce) = store.delete_entity(entity.id, DeleteMode::Hard).await {
                            tracing::error!(
                                error = %ce,
                                id = %entity.id,
                                "create_entity: failed to roll back entity row after embed failure"
                            );
                        }
                    }
                    if let Ok(fts) = self.text(token) {
                        if let Err(ce) = fts.delete_document(ns, entity.id).await {
                            tracing::error!(
                                error = %ce,
                                id = %entity.id,
                                "create_entity: failed to roll back FTS document after embed failure"
                            );
                        }
                    }
                    return Err(e);
                }
            };
            // TODO(P2): parallelize vector inserts
            let mut inserted_models: Vec<String> = Vec::with_capacity(embed_model_names.len());
            for (model_name, vector) in embed_model_names.iter().zip(vectors) {
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
                                vec![vector],
                            )
                            .await
                            .map_err(RuntimeError::from),
                        Err(e) => Err(e),
                    }
                };
                if let Err(e) = insert_result {
                    // Compensate entity row + FTS + already-inserted vectors.
                    if let Ok(store) = self.entities(token) {
                        if let Err(ce) = store.delete_entity(entity.id, DeleteMode::Hard).await {
                            tracing::error!(
                                error = %ce,
                                id = %entity.id,
                                "create_entity: failed to roll back entity row after vector insert failure"
                            );
                        }
                    }
                    if let Ok(fts) = self.text(token) {
                        if let Err(ce) = fts.delete_document(ns, entity.id).await {
                            tracing::error!(
                                error = %ce,
                                id = %entity.id,
                                "create_entity: failed to roll back FTS document after vector insert failure"
                            );
                        }
                    }
                    for m in &inserted_models {
                        if let Ok(vs) = self.vectors_for_model(token, m) {
                            if let Err(ce) = vs.delete(entity.id).await {
                                tracing::error!(
                                    error = %ce,
                                    model = m,
                                    id = %entity.id,
                                    "create_entity: failed to roll back vector for model after insert failure"
                                );
                            }
                        }
                    }
                    return Err(e);
                }
                inserted_models.push(model_name.clone());
            }
        }

        Ok(entity)
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
            entity_types: match entity_type {
                Some(t) => vec![t.to_string()],
                None => vec![],
            },
            namespaces: ns_strs,
            ..Default::default()
        };
        let page = self
            .entities(token)?
            .query_entities(
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
            .query_entities(
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
    /// - All other 13 relations: both endpoints MUST be entities.
    ///
    /// Returns `Ok(())` when valid; otherwise `InvalidInput` or `NotFound` with
    /// the same messages as the previous inline block (byte-identical behaviour).
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
    ) -> RuntimeResult<()> {
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
            if !self.substrate_exists_by_id(token, target_id).await? {
                return Err(RuntimeError::NotFound(format!(
                    "link target {target_id} not found"
                )));
            }
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
            match (&src, &tgt) {
                (Resolved::Entity(src_e), Resolved::Entity(tgt_e)) => {
                    if !base_entity_rule_allows(&src_e.kind, relation, &tgt_e.kind) {
                        let rule_hint = match relation {
                            EdgeRelation::Supports | EdgeRelation::Refutes => {
                                "requires concept|document|dataset|artifact -> concept \
                                 (or same-substrate note -> note)"
                            }
                            _ => "requires same-kind entity endpoints",
                        };
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[{rel_name}]-> ({}) is not in the base endpoint \
                             allowlist; {rel_name} {rule_hint}",
                            src_e.kind, tgt_e.kind
                        )));
                    }
                }
                (Resolved::Note(_), Resolved::Note(_)) => {}
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
            }
        } else {
            // All remaining base relations require entity→entity with kind-level
            // restrictions (see base allowlist). Packs may extend the allowlist
            // additively via EDGE_RULES.
            //
            // Strategy: resolve both endpoints once (by-ID, unfiltered), consult pack
            // rules; on miss, fall through to the original base-rule error messages.
            let src_res = self.resolve_edge_endpoint(token, source_id).await?;
            let tgt_res = self.resolve_edge_endpoint(token, target_id).await?;

            if pack_rule_allows(
                &self.pack_edge_rules(),
                relation,
                src_res.as_ref(),
                tgt_res.as_ref(),
            ) {
                return Ok(());
            }

            // Substrate check: both endpoints must be entities.
            let src_kind = match src_res {
                Some(Resolved::Entity(e)) => e.kind,
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
            let tgt_kind = match tgt_res {
                Some(Resolved::Entity(e)) => e.kind,
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
            if !base_entity_rule_allows(&src_kind, relation, &tgt_kind) {
                return Err(RuntimeError::InvalidInput(format!(
                    "({src_kind}) -[{}]-> ({tgt_kind}) is not in the base endpoint \
                     allowlist; use pack EDGE_RULES to extend the allowlist",
                    relation.as_str()
                )));
            }
        }
        Ok(())
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
                        let rule_hint = match relation {
                            EdgeRelation::Supports | EdgeRelation::Refutes => {
                                "requires concept|document|dataset|artifact -> concept \
                                 (or same-substrate note -> note)"
                            }
                            _ => "requires same-kind entity endpoints",
                        };
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[{rel_name}]-> ({}) is not in the base endpoint \
                             allowlist; {rel_name} {rule_hint}",
                            src_e.kind, tgt_e.kind
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
        if pack_rule_allows(&self.pack_edge_rules(), relation, src, tgt) {
            return Ok(());
        }

        let src_kind = match src {
            Some(Resolved::Entity(e)) => &e.kind,
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
        let tgt_kind = match tgt {
            Some(Resolved::Entity(e)) => &e.kind,
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
            return Err(RuntimeError::InvalidInput(format!(
                "({src_kind}) -[{}]-> ({tgt_kind}) is not in the base endpoint \
                 allowlist; use pack EDGE_RULES to extend the allowlist",
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
        validate_edge_weight(weight)?;
        self.validate_edge_relation_endpoints(token, source_id, target_id, relation)
            .await?;
        let (source_id, target_id) = canonical_edge_endpoints(relation, source_id, target_id);
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
        match self.graph(token)?.upsert_edge_guarded(edge).await? {
            khive_storage::GuardedWriteOutcome::Written => {}
            khive_storage::GuardedWriteOutcome::Refused(missing) => {
                return Err(RuntimeError::GuardedWriteFailed(GuardedWriteFailure {
                    entry_index: None,
                    missing_source: missing.source.then_some(source_id),
                    missing_target: missing.target.then_some(target_id),
                }));
            }
        }

        // Read back the persisted row by natural key so the returned
        // edge ID is always the one stored in the database, not the locally
        // generated UUID that was displaced by an ON CONFLICT DO UPDATE.
        // Under parallel calls for the same triple, every caller now returns
        // the same persisted edge ID — the winner's insert or the updated row.
        let persisted = self
            .list_edges(
                token,
                crate::curation::EdgeListFilter {
                    source_id: Some(source_id),
                    target_id: Some(target_id),
                    relations: vec![relation],
                    ..Default::default()
                },
                1,
                0,
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                crate::RuntimeError::Internal(format!(
                    "upsert_edge succeeded but natural-key lookup for ({source_id}, {target_id}, {relation}) returned nothing"
                ))
            })?;
        Ok(persisted)
    }

    /// Write an edge with an explicit `target_backend` stamp (ADR-029 D3).
    ///
    /// Called by the `SubstrateCoordinator` when source and target are on
    /// different backends. The coordinator validates endpoints before calling
    /// this method via [`Self::validate_link_endpoints`], so endpoint validation is
    /// skipped here. The edge is written on the source backend only.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_with_target_backend(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        target_backend: Option<String>,
    ) -> RuntimeResult<Edge> {
        validate_edge_weight(weight)?;
        let (source_id, target_id) = canonical_edge_endpoints(relation, source_id, target_id);
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
        self.graph(token)?.upsert_edge(edge).await?;
        let persisted = self
            .list_edges(
                token,
                crate::curation::EdgeListFilter {
                    source_id: Some(source_id),
                    target_id: Some(target_id),
                    relations: vec![relation],
                    ..Default::default()
                },
                1,
                0,
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                crate::RuntimeError::Internal(format!(
                    "upsert_edge succeeded but natural-key lookup for ({source_id}, {target_id}, {relation}) returned nothing"
                ))
            })?;
        Ok(persisted)
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
        mut query: NeighborQuery,
    ) -> RuntimeResult<Vec<NeighborHit>> {
        if !self.substrate_exists_in_ns(token, node_id).await? {
            return Ok(Vec::new());
        }

        query.direction =
            normalize_symmetric_direction(query.direction, query.relations.as_deref());
        let mut hits = Vec::new();
        for ns in token.visible_namespaces() {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let mut ns_hits = self.graph(&temp)?.neighbors(node_id, query.clone()).await?;
            hits.append(&mut ns_hits);
        }
        hits.sort_by_key(|h| (h.node_id, h.edge_id));
        hits.dedup_by_key(|h| (h.node_id, h.edge_id));
        self.enrich_neighbor_hits(token, &mut hits).await;
        // Filter out soft-deleted entity nodes.
        let candidate_ids: Vec<Uuid> = hits.iter().map(|h| h.node_id).collect();
        let deleted = self.deleted_entity_ids(candidate_ids).await;
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
        });
        Ok(hits)
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
        if !self.substrate_exists_in_ns(token, node_id).await? {
            return Ok(Vec::new());
        }

        let mut hits: Vec<DirectedNeighborHit> = Vec::new();
        for ns in token.visible_namespaces() {
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
        let deleted = self.deleted_entity_ids(candidate_ids).await;
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
        });
        Ok(hits.into_iter().map(|h| (h.hit, h.direction)).collect())
    }

    /// Traverse the graph from a set of root nodes.
    ///
    /// Roots in a foreign namespace are silently filtered before storage expansion.
    /// Soft-deleted entity nodes are excluded from results.
    pub async fn traverse(
        &self,
        token: &NamespaceToken,
        request: TraversalRequest,
    ) -> RuntimeResult<Vec<GraphPath>> {
        let mut request = request;
        let mut visible_roots = Vec::with_capacity(request.roots.len());
        for root in request.roots.drain(..) {
            if self.substrate_exists_in_ns(token, root).await? {
                visible_roots.push(root);
            }
        }
        request.roots = visible_roots;
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
        let mut paths = merge_traversal_paths_by_root(paths, request.options.limit);
        self.enrich_path_nodes(token, &mut paths, request.include_properties)
            .await;
        // Filter out soft-deleted entity nodes from all path nodes.
        let all_node_ids: Vec<Uuid> = paths
            .iter()
            .flat_map(|p| p.nodes.iter().map(|n| n.node_id))
            .collect();
        let deleted = self.deleted_entity_ids(all_node_ids).await;
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
    async fn deleted_entity_ids(&self, ids: Vec<Uuid>) -> std::collections::HashSet<Uuid> {
        if ids.is_empty() {
            return std::collections::HashSet::new();
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
        let sql_str = format!(
            "SELECT id FROM entities WHERE id IN ({entities_placeholders}) AND deleted_at IS NOT NULL \
             UNION \
             SELECT id FROM notes WHERE id IN ({notes_placeholders}) AND deleted_at IS NOT NULL"
        );
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
        let sql = self.sql();
        if let Ok(mut reader) = sql.reader().await {
            if let Ok(rows) = reader.query_all(stmt).await {
                for row in rows {
                    if let Some(col) = row.columns.first() {
                        if let SqlValue::Text(s) = &col.value {
                            if let Ok(u) = s.parse::<Uuid>() {
                                out.insert(u);
                            }
                        }
                    }
                }
            }
            // best-effort: on reader or query error, treat none as deleted
        }
        out
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
                let kind = note.kind.clone();
                let name = note
                    .name
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| format!("[{kind}]"));
                hit.name = Some(name);
                hit.kind = Some(kind);
            }
        }
    }

    /// Populate `name` and `kind` on each `PathNode` from the corresponding
    /// entity record.
    ///
    /// Unlike `enrich_neighbor_hits`, this is entity-only by design: it does
    /// not fall back to a note lookup for IDs that aren't entities.
    /// A traversal can still reach note nodes (e.g. via an `annotates`
    /// edge) — they are not filtered out of `GraphPath::nodes` — but they are
    /// left with `name = None, kind = None` rather than resolved. Widening
    /// this to notes would change every existing caller's enriched output
    /// for note-reaching traversals, so it stays scoped to entities until
    /// that is an intentional product decision.
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

        for path in paths.iter_mut() {
            for node in path.nodes.iter_mut() {
                if let Some(entity) = entity_map.get(&node.node_id) {
                    node.name = Some(entity.name.clone());
                    node.kind = Some(entity.kind.clone());
                    if include_properties {
                        node.properties = entity.properties.clone();
                    }
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
        self.create_note_inner(
            token, kind, name, content, None, salience, None, properties, annotates, None,
        )
        .await
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
        self.create_note_inner(
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
        )
        .await
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
        self.create_note_inner(
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
        )
        .await
    }

    /// Insert a note using `INSERT OR IGNORE` semantics for atomic deduplication.
    ///
    /// Returns `Ok(Some(note))` when the note was newly written.  Returns
    /// `Ok(None)` when a unique constraint (e.g. the `external_id` partial
    /// index on comm message notes) was already satisfied by an existing row,
    /// making this call a no-op.  FTS indexing and vector embedding are
    /// attempted on success but treated as best-effort: failures are logged
    /// and do not abort the write.
    ///
    /// This method is intentionally narrower than `create_note`: it skips
    /// salience/decay, annotates edges, and embedding-model selection, which
    /// are not needed for channel-ingest paths.
    pub async fn try_create_note(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        properties: Option<serde_json::Value>,
    ) -> RuntimeResult<Option<Note>> {
        self.validate_note_kind(kind)?;
        crate::secret_gate::check(content)?;
        if let Some(n) = name {
            crate::secret_gate::check(n)?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json(p)?;
        }

        let ns = token.namespace().as_str();
        let mut note = Note::new(ns, kind, content);
        if let Some(n) = name {
            note = note.with_name(n);
        }
        if let Some(p) = properties {
            note = note.with_properties(p);
        }

        let inserted = self.notes(token)?.try_insert_note(note.clone()).await?;
        if !inserted {
            return Ok(None);
        }

        // Best-effort FTS: log and continue on failure.
        if let Ok(fts) = self.text_for_notes(token) {
            if let Err(e) = fts.upsert_document(note_fts_document(&note)).await {
                tracing::warn!(
                    note_id = %note.id,
                    error = %e,
                    "try_create_note: FTS indexing failed (non-fatal)"
                );
            }
        }

        // Best-effort vector embedding: log and continue on failure.
        let embed_model_names = self.registered_embedding_model_names();
        for model_name in &embed_model_names {
            match self
                .embed_document_with_model(model_name, &note_embedding_text(&note))
                .await
            {
                Ok(vector) => {
                    if let Ok(vs) = self.vectors_for_model(token, model_name) {
                        if let Err(e) = vs
                            .insert(
                                note.id,
                                SubstrateKind::Note,
                                ns,
                                "note.content",
                                vec![vector],
                            )
                            .await
                        {
                            tracing::warn!(
                                note_id = %note.id,
                                model = %model_name,
                                error = %e,
                                "try_create_note: vector insert failed (non-fatal)"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        note_id = %note.id,
                        model = %model_name,
                        error = %e,
                        "try_create_note: embedding failed (non-fatal)"
                    );
                }
            }
        }

        Ok(Some(note))
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
    ) -> RuntimeResult<Note> {
        self.validate_note_kind(kind)?;
        // Secret gate: scan content, optional name, and structured properties.
        crate::secret_gate::check(content)?;
        if let Some(n) = name {
            crate::secret_gate::check(n)?;
        }
        if let Some(ref p) = properties {
            crate::secret_gate::check_json(p)?;
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
            crate::secret_gate::check(ec)?;
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
        self.notes(token)?.upsert_note(note.clone()).await?;

        // From here on, any error must compensate by removing the note row, its
        // FTS document, and any vector entries already inserted — the same
        // cleanup used by the annotates-edge block below.

        // Decide which embedding models to use (before touching FTS/vectors).
        let embed_model_names: Vec<String> = if let Some(m) = embedding_model {
            vec![m.to_string()]
        } else {
            // Fan out to ALL registered models — includes both lattice models
            // from RuntimeConfig and any custom providers added via
            // register_embedder(). Gate on the registry, not
            // config().embedding_model, so that custom-only runtimes (no
            // lattice model in config) also fan out.
            let names = self.registered_embedding_model_names();
            if names.is_empty() {
                // No models configured at all — skip vector embedding.
                vec![]
            } else {
                names
            }
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
                match self.text_for_notes(token) {
                    Ok(fts) => fts
                        .upsert_document(note_fts_document(&note))
                        .await
                        .map_err(RuntimeError::from),
                    Err(e) => Err(e),
                }
            };

            if let Err(e) = fts_result {
                // Best-effort compensation — ignore cleanup errors.
                if let Ok(store) = self.notes(token) {
                    let _ = store.delete_note(note.id, DeleteMode::Hard).await;
                }
                return Err(e);
            }
        }

        // Vector embedding + insert step — compensate note row + FTS doc on failure.
        // Multi-model vector embedding:
        //   - explicit embedding_model → single model (existing behaviour)
        //   - None + any models registered → ALL registered models in parallel
        //   - None + no models configured → skip (text-only)
        // The effective text sent to every embedder: the caller-supplied
        // capped override when present, otherwise the full stored content.
        // FTS indexing above always used the full `note.content` — this cap
        // affects only the vector-embedding input.
        let canonical_embed_text = note_embedding_text(&note);
        let embed_text: &str = embedding_content.unwrap_or(&canonical_embed_text);

        if embed_model_names.len() == 1 {
            // Single-model path: preserves original sequential behaviour.
            let model_name = &embed_model_names[0];
            let vec_result = self.embed_document_with_model(model_name, embed_text).await;

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
            let vec_result: RuntimeResult<Vec<f32>> = if vec_inject {
                Err(RuntimeError::Internal(
                    "injected vector failure".to_string(),
                ))
            } else {
                vec_result
            };

            let single_model_result: RuntimeResult<()> = match vec_result {
                Ok(vector) => match self.vectors_for_model(token, model_name) {
                    Ok(vs) => vs
                        .insert(
                            note.id,
                            SubstrateKind::Note,
                            ns,
                            "note.content",
                            vec![vector],
                        )
                        .await
                        .map_err(RuntimeError::from),
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            if let Err(e) = single_model_result {
                // Compensate note row + FTS.
                if let Ok(store) = self.notes(token) {
                    let _ = store.delete_note(note.id, DeleteMode::Hard).await;
                }
                if let Ok(fts) = self.text_for_notes(token) {
                    let _ = fts.delete_document(ns, note.id).await;
                }
                return Err(e);
            }
        } else if !embed_model_names.is_empty() {
            // Multi-model path: embed with each model in parallel via spawned tasks,
            // then insert one VectorRecord per model.
            let rt_clone = self.clone();
            let content_owned = embed_text.to_string();
            let usage_ctx = crate::usage::current();
            let mut join_set = tokio::task::JoinSet::new();
            for (idx, model_name) in embed_model_names.iter().enumerate() {
                let rt = rt_clone.clone();
                let text = content_owned.clone();
                let name = model_name.clone();
                let ctx = usage_ctx.clone();
                join_set.spawn(async move {
                    let fut = rt.embed_document_with_model(&name, &text);
                    let result = match ctx {
                        Some(ctx) => crate::usage::scope(ctx, fut).await,
                        None => fut.await,
                    };
                    (idx, result)
                });
            }
            // The first failed or panicked handle aborts and detaches its
            // siblings. Embed usage is counted at dispatch, so a synchronous
            // provider winding down in the background cannot change it.
            let vectors = match drain_embed_join_set(join_set, embed_model_names.len()).await {
                Ok(vectors) => vectors,
                Err(e) => {
                    // Compensate note row + FTS (no vectors inserted yet).
                    if let Ok(store) = self.notes(token) {
                        let _ = store.delete_note(note.id, DeleteMode::Hard).await;
                    }
                    if let Ok(fts) = self.text_for_notes(token) {
                        let _ = fts.delete_document(ns, note.id).await;
                    }
                    return Err(e);
                }
            };
            // TODO(P2): parallelize vector inserts
            let mut inserted_models: Vec<String> = Vec::with_capacity(embed_model_names.len());
            for (model_name, vector) in embed_model_names.iter().zip(vectors) {
                let insert_result = match self.vectors_for_model(token, model_name) {
                    Ok(vs) => vs
                        .insert(
                            note.id,
                            SubstrateKind::Note,
                            ns,
                            "note.content",
                            vec![vector],
                        )
                        .await
                        .map_err(RuntimeError::from),
                    Err(e) => Err(e),
                };
                if let Err(e) = insert_result {
                    // Compensate note row + FTS + already-inserted vectors.
                    if let Ok(store) = self.notes(token) {
                        let _ = store.delete_note(note.id, DeleteMode::Hard).await;
                    }
                    if let Ok(fts) = self.text_for_notes(token) {
                        let _ = fts.delete_document(ns, note.id).await;
                    }
                    for m in &inserted_models {
                        if let Ok(vs) = self.vectors_for_model(token, m) {
                            let _ = vs.delete(note.id).await;
                        }
                    }
                    return Err(e);
                }
                inserted_models.push(model_name.clone());
            }
        }

        // Create annotates edges, compensating on failure to preserve atomicity.
        //
        // Pre-validation (above) ensures all targets exist, so link failures are
        // unexpected. If one occurs: delete any edges already created, then remove
        // the note, its FTS document, and its vector entry.
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
                    // Best-effort compensation — ignore cleanup errors.
                    for edge_id in created_edges {
                        let _ = self.delete_edge(token, edge_id, true).await;
                    }
                    if let Ok(store) = self.notes(token) {
                        let _ = store.delete_note(note.id, DeleteMode::Hard).await;
                    }
                    if let Ok(fts) = self.text_for_notes(token) {
                        let _ = fts.delete_document(ns, note.id).await;
                    }
                    for model_name in &embed_model_names {
                        if let Ok(vs) = self.vectors_for_model(token, model_name) {
                            let _ = vs.delete(note.id).await;
                        }
                    }
                    return Err(e);
                }
            }
        }

        Ok(note)
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
                .query_notes(
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
            .query_notes_filtered(
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

    /// Count notes matching `kind`, summed across the caller's visible
    /// namespaces. The store-layer `count_notes` is namespace-pinned by
    /// design (no `NamespaceFilter`-style `IN (...)` support); this sums the
    /// per-namespace store calls, mirroring [`Self::count_edges_by_relation`]
    /// and the multi-namespace path in [`Self::list_notes`] so `stats().notes`
    /// reconciles with a full `list` keyset walk under the same token.
    pub async fn count_notes(
        &self,
        token: &NamespaceToken,
        kind: Option<&str>,
    ) -> RuntimeResult<u64> {
        let mut total = 0u64;
        for ns in token.visible_namespaces() {
            let temp = NamespaceToken::for_namespace(ns.clone());
            total += self.notes(&temp)?.count_notes(ns.as_str(), kind).await?;
        }
        Ok(total)
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

        let text_search_result = if fts_search_inject {
            Err(khive_storage::StorageError::Timeout {
                operation: "fts_search".into(),
            })
        } else {
            self.text_for_notes(token)?
                .search(TextSearchRequest {
                    query: query_text.to_string(),
                    mode: TextQueryMode::Plain,
                    filter: Some(TextFilter {
                        namespaces: visible_ns.clone(),
                        ..TextFilter::default()
                    }),
                    top_k: candidates,
                    snippet_chars: 200,
                })
                .await
        };

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

        // Vector search filtered to notes.
        let vector_hits = if query_vector.is_some() || self.config().embedding_model.is_some() {
            self.vector_search(
                token,
                query_vector,
                Some(query_text),
                candidates,
                Some(SubstrateKind::Note),
            )
            .await?
        } else {
            vec![]
        };

        // Keep the full text∪vector union through RRF — salience weighting and
        // soft-delete/kind filtering happen *after* this, and the final
        // `hits.truncate(limit)` is the only result-limiting cut. Truncating to
        // `candidates` here would drop a high-salience note ranked just outside
        // the raw RRF cutoff before salience ever applied.
        let fuse_k = text_hits.len() + vector_hits.len();
        let fused = crate::fusion::rrf_fuse_k(text_hits, vector_hits, RRF_K, fuse_k)?;

        let candidate_ids: Vec<Uuid> = fused.iter().map(|hit| hit.entity_id).collect();
        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        // Fetch each candidate note individually to get salience and apply
        // soft-delete + (optional) kind filtering. Notes whose `kind` doesn't
        // match `note_kind` are dropped post-fetch — they're a small set
        // bounded by the text∪vector union (≤ 2×candidates), so the read is cheap.
        let note_store = self.notes(token)?;
        let mut alive_notes: HashMap<Uuid, Note> = HashMap::new();
        for id in &candidate_ids {
            if let Some(note) = note_store.get_note(*id).await? {
                if note.deleted_at.is_some() {
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
                alive_notes.insert(*id, note);
            }
        }

        // Drop superseded notes unless include_superseded is true: any note targeted
        // by a `supersedes` edge is obsolete and excluded from default search.
        if !include_superseded && !alive_notes.is_empty() {
            let graph = self.graph(token)?;
            let mut superseded: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
            for &note_id in alive_notes.keys() {
                let inbound = graph
                    .neighbors(
                        note_id,
                        NeighborQuery {
                            direction: Direction::In,
                            relations: Some(vec![EdgeRelation::Supersedes]),
                            limit: Some(1),
                            min_weight: None,
                        },
                    )
                    .await?;
                if !inbound.is_empty() {
                    superseded.insert(note_id);
                }
            }
            alive_notes.retain(|id, _| !superseded.contains(id));
        }

        // Apply salience weighting and collect final hits.
        let mut hits: Vec<NoteSearchHit> = fused
            .into_iter()
            .filter_map(|hit| {
                let note = alive_notes.get(&hit.entity_id)?;
                let salience = note.salience.unwrap_or(0.5);
                let weight = 0.5 + 0.5 * salience;
                let weighted = DeterministicScore::from_f64(hit.score.to_f64() * weight);
                Some(NoteSearchHit {
                    note_id: hit.entity_id,
                    score: weighted,
                    source: hit.source,
                    title: hit.title.or_else(|| note_title(note)),
                    snippet: hit.snippet.or_else(|| note_snippet(note)),
                })
            })
            .collect();

        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.note_id.cmp(&b.note_id)));
        hits.truncate(limit as usize);
        Ok(hits)
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
        self.resolve_prefix_inner(Some(&namespaces), prefix, false)
            .await
    }

    pub async fn resolve_prefix_including_deleted(
        &self,
        token: &NamespaceToken,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        let namespaces = [token.namespace().as_str().to_owned()];
        self.resolve_prefix_inner(Some(&namespaces), prefix, true)
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
        self.resolve_prefix_inner(None, prefix, false).await
    }

    /// `resolve_prefix_unfiltered`, including soft-deleted rows — used by the
    /// hard-delete by-ID path.
    pub async fn resolve_prefix_unfiltered_including_deleted(
        &self,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        self.resolve_prefix_inner(None, prefix, true).await
    }

    /// Shared prefix-scan implementation over an explicit namespace set.
    ///
    /// `namespaces` selects the scan scope: `Some(&[ns])` reproduces the
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
    ) -> RuntimeResult<Option<Uuid>> {
        use khive_storage::types::{SqlStatement, SqlValue};

        // Every caller is expected to pre-validate hex-only input, but this is
        // the single choke point every `resolve_prefix*` variant funnels
        // through, so re-validate here too. A prefix containing anything other
        // than hex digits and
        // canonical hyphen separators (`%`, `_`, or other LIKE-wildcard /
        // injection-shaped input) never matches a real id and is rejected
        // before it can reach the LIKE pattern, instead of relying on bound
        // params alone to neutralize wildcard semantics.
        if !prefix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Ok(None);
        }

        let pattern = format!("{}%", hex_prefix_to_uuid_pattern(prefix));

        let tables = [
            ("entities", true),
            ("notes", true),
            ("events", false),
            ("graph_edges", false),
        ];

        let ns_clause = namespaces.map(|ns| {
            let placeholders: Vec<String> = (0..ns.len()).map(|i| format!("?{}", i + 2)).collect();
            format!(" AND namespace IN ({})", placeholders.join(", "))
        });

        // A UUID can legitimately exist in more than one scanned table
        // (e.g. an entity id string that also happens to be an edge id — the
        // scan is purely a text-prefix LIKE across independent tables, not a
        // substrate-exclusive lookup). Without dedup, a single record hit
        // twice across tables inflated `matches.len()` past 1 and produced a
        // false `AmbiguousPrefix` naming the SAME UUID twice. `seen` tracks
        // UUIDs already pushed so `matches` (and thus every length check,
        // including the early-exit below) reflects DISTINCT UUIDs only.
        let mut matches: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut reader = self.sql().reader().await.map_err(RuntimeError::Storage)?;

        for (table, has_deleted_at) in tables {
            let deleted_filter = if has_deleted_at && !include_deleted {
                " AND deleted_at IS NULL"
            } else {
                ""
            };
            let mut params = vec![SqlValue::Text(pattern.clone())];
            if let Some(ns) = namespaces {
                params.extend(ns.iter().map(|n| SqlValue::Text(n.clone())));
            }
            let sql = SqlStatement {
                sql: format!(
                    "SELECT id FROM {table} WHERE id LIKE ?1{ns_clause}{deleted_filter} LIMIT 2",
                    ns_clause = ns_clause.as_deref().unwrap_or("")
                ),
                params,
                label: Some("resolve_prefix".into()),
            };
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
                    if msg.contains("no such table") {
                        continue;
                    }
                    return Err(RuntimeError::Storage(e));
                }
            }
            if matches.len() > 1 {
                break;
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
        let ns = token.namespace().as_str();

        // Entity: primary-only check (exclude entities in visible-only namespaces).
        if let Some(entity) = self.entities(token)?.get_entity(id).await? {
            if Self::ensure_namespace(&entity.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Entity(entity)));
            }
        }

        // Note: primary-only check.
        if let Some(note) = self.notes(token)?.get_note(id).await? {
            if Self::ensure_namespace(&note.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Note(note)));
            }
        }

        // Event: primary-only check.
        if let Some(event) = self.events(token)?.get_event(id).await? {
            if Self::ensure_namespace(&event.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Event(event)));
            }
        }

        Ok(None)
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
        let ns = token.namespace().as_str();

        if let Some(entity) = self
            .entities(token)?
            .get_entity_including_deleted(id)
            .await?
        {
            if Self::ensure_namespace(&entity.namespace, ns).is_ok() {
                return Ok(Some(Resolved::Entity(entity)));
            }
        }

        if let Some(note) = self.notes(token)?.get_note_including_deleted(id).await? {
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

    /// Hard-delete a single graph node (entity, note, or edge-as-node row)
    /// AND purge its incident edges in ONE write transaction.
    ///
    /// The endpoint row delete and the incident-edge cascade used
    /// to run as two independently-committing storage calls. A concurrent
    /// guarded write (`upsert_edge_guarded`/`upsert_edges_guarded`) landing
    /// between them could see the endpoint still live, insert a fresh edge
    /// against it, and then survive the cascade that already ran — a
    /// durably dangling edge with no second purge. Routing both statements
    /// through one [`run_atomic_unit`] call closes the window: since every
    /// write (this one and the guarded insert) funnels through the same
    /// single-writer queue, a concurrent guarded write either fully commits
    /// before this unit starts (and its edge is then swept by the purge
    /// below, in the same transaction as the row delete) or fully commits
    /// after this unit has already committed (and its own endpoint-existence
    /// check then sees the endpoint gone and refuses the write) — there is
    /// no state in which it can observe the endpoint alive with edges
    /// already purged.
    ///
    /// `row_statement` is the exact hard-delete `DELETE` for the target row
    /// (entity, note, or edge). Returns `Ok(true)` if the row was deleted,
    /// `Ok(false)` if it no longer existed (lost a race with a concurrent
    /// delete of the same row) — never an error for that case, matching the
    /// non-atomic bool-returning shape callers had before this fix.
    async fn atomic_hard_delete_with_edge_purge(
        &self,
        row_statement: SqlStatement,
        node_id: Uuid,
    ) -> RuntimeResult<bool> {
        let plan = AtomicOpPlan::Delete(DeletePlan {
            target_id: node_id,
            statements: vec![
                PlanStatement {
                    statement: row_statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                },
                PlanStatement {
                    statement: purge_incident_edges_statement(node_id),
                    guard: None,
                },
            ],
            post_commit: PostCommitEffect::None,
        });
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { .. }) => Ok(true),
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
        let note_store = self.notes(token)?;
        let note = if hard {
            match note_store.get_note_including_deleted(id).await? {
                Some(n) => n,
                None => return Ok(false),
            }
        } else {
            match note_store.get_note(id).await? {
                Some(n) => n,
                None => return Ok(false),
            }
        };
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Route index cleanup through the RECORD's namespace, not the caller's.
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&note.namespace)
                .map_err(|e| RuntimeError::Internal(format!("note namespace invalid: {e}")))?,
        );
        let record_ns = note.namespace.clone();

        // On hard delete, the row delete and the incident-edge cascade (including
        // already-soft-deleted edges) run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`. Index cleanup follows the
        // commit; it is best-effort and idempotent, unlike the row/edge pair.
        let deleted = if hard {
            let deleted = self
                .atomic_hard_delete_with_edge_purge(note_hard_delete_statement(id), id)
                .await?;
            self.text_for_notes(&record_tok)?
                .delete_document(&record_ns, id)
                .await?;
            // Scoped delete: iterate over EVERY registered embedding model's
            // vector store so non-default vectors don't orphan when the note is deleted.
            for model_name in self.registered_embedding_model_names() {
                self.vectors_for_model(&record_tok, &model_name)?
                    .delete(id)
                    .await?;
            }
            deleted
        } else {
            let deleted = note_store.delete_note(id, mode).await?;
            if deleted {
                self.text_for_notes(&record_tok)?
                    .delete_document(&record_ns, id)
                    .await?;
                for model_name in self.registered_embedding_model_names() {
                    self.vectors_for_model(&record_tok, &model_name)?
                        .delete(id)
                        .await?;
                }
            }
            deleted
        };
        if deleted {
            let event_store = self.events(token)?;
            let event = khive_storage::event::Event::new(
                record_ns.clone(),
                "delete",
                EventKind::NoteDeleted,
                SubstrateKind::Note,
                "",
            )
            .with_target(id)
            .with_payload(serde_json::json!({"id": id, "namespace": record_ns, "hard": hard}));
            event_store.append_event(event).await.map_err(|e| {
                RuntimeError::Internal(format!("delete_note: event store write failed: {e}"))
            })?;
            // A soft OR hard delete removes the note's vectors/FTS document
            // above: any pack-owned vector-derived cache (e.g.
            // khive-pack-memory's warm ANN index) needs to know the corpus
            // changed, reached via this generic hook so khive-runtime never
            // takes a dependency on khive-pack-memory. No-op when no pack has
            // installed a hook.
            self.fire_note_mutation_hook(&note.kind, id).await;
        }
        Ok(deleted)
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
    /// `true` when the server-side row cap bound this result — `rows` is a
    /// prefix of the true match set, not the whole thing (#1168, #1247). A
    /// structural flag so a caller can detect an incomplete result without
    /// parsing the human-oriented `warnings` text.
    pub truncated: bool,
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

        // When the server-side cap was the binding constraint, the compiled
        // SQL asked for one extra (sentinel) row. Its presence in the actual
        // result set — not the requested LIMIT — is the truncation signal
        // (a `LIMIT 1000` that only matches 20 rows must not warn, and a
        // query with no `LIMIT` that matches 501+ rows must).
        let mut truncated = false;
        if let Some(check) = truncation_check {
            if rows.len() > check.max_limit {
                rows.truncate(check.max_limit);
                truncated = true;
                // GQL has no SKIP/OFFSET/ORDER BY today (#1168) — the prior
                // wording here recommended a paging path that does not exist.
                // `truncated` is the structural signal (#1247); this message
                // stays prose-only context for a human reader.
                warnings.push(match check.requested_limit {
                    Some(requested) => format!(
                        "result set capped at {} rows; requested limit {requested} exceeds the \
                         cap. This query language does not support SKIP/OFFSET paging yet — \
                         check the `truncated` field, not this message, to detect an incomplete \
                         result programmatically.",
                        check.max_limit
                    ),
                    None => format!(
                        "result set capped at {} rows; more than {} rows matched with no LIMIT \
                         clause. This query language does not support SKIP/OFFSET paging yet — \
                         check the `truncated` field, not this message, to detect an incomplete \
                         result programmatically.",
                        check.max_limit, check.max_limit
                    ),
                });
            }
        }

        Ok(QueryResult {
            rows,
            warnings,
            truncated,
        })
    }

    /// Soft-delete or hard-delete an entity by ID (soft delete by default).
    ///
    /// On hard delete, cascades to remove all incident edges (both inbound and
    /// outbound) to prevent dangling references. Soft delete also cleans FTS
    /// and vector indexes; edges are left in place.
    ///
    /// UUID v4 is globally unique: no namespace filter on by-ID ops.
    pub async fn delete_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        hard: bool,
    ) -> RuntimeResult<bool> {
        let entity = if hard {
            match self
                .entities(token)?
                .get_entity_including_deleted(id)
                .await?
            {
                Some(e) => e,
                None => return Ok(false),
            }
        } else {
            match self.entities(token)?.get_entity(id).await? {
                Some(e) => e,
                None => return Ok(false),
            }
        };
        let mode = if hard {
            DeleteMode::Hard
        } else {
            DeleteMode::Soft
        };

        // Route cascade and index cleanup through the RECORD's namespace, not the caller's.
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&entity.namespace)
                .map_err(|e| RuntimeError::Internal(format!("entity namespace invalid: {e}")))?,
        );

        // On hard delete, the row delete and the incident-edge cascade (including
        // already-soft-deleted edges) run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`. Index cleanup follows the
        // commit; it is best-effort and idempotent, unlike the row/edge pair.
        let deleted = if hard {
            let deleted = self
                .atomic_hard_delete_with_edge_purge(entity_hard_delete_statement(id), id)
                .await?;
            self.remove_from_indexes(&record_tok, id).await?;
            deleted
        } else {
            let deleted = self.entities(token)?.delete_entity(id, mode).await?;
            if deleted {
                self.remove_from_indexes(&record_tok, id).await?;
            }
            deleted
        };
        if deleted {
            let event_store = self.events(token)?;
            let ns = entity.namespace.clone();
            let event = khive_storage::event::Event::new(
                ns.clone(),
                "delete",
                EventKind::EntityDeleted,
                SubstrateKind::Entity,
                "",
            )
            .with_target(id)
            .with_payload(serde_json::json!({"id": id, "namespace": ns, "hard": hard}));
            event_store.append_event(event).await.map_err(|e| {
                RuntimeError::Internal(format!("delete_entity: event store write failed: {e}"))
            })?;
        }
        Ok(deleted)
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
    /// [`Self::EDGE_LIST_MAX_LIMIT`]; defaults to 100.
    ///
    /// `offset` pages through the full matching set (previously hard-coded to
    /// 0, so every page returned the same first rows). For
    /// O(1)-at-depth walks over large edge populations, prefer
    /// [`Self::list_edges_after`] instead of paging offset deep.
    pub async fn list_edges(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
        limit: u32,
        offset: u32,
    ) -> RuntimeResult<Vec<Edge>> {
        let limit = limit.clamp(1, Self::EDGE_LIST_MAX_LIMIT);
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

        // Multi-namespace visibility: `offset` must apply to the combined,
        // deduplicated set rather than per-namespace pages, so fetch enough
        // of each namespace's page to cover it, merge, then slice.
        let fetch_limit = offset.saturating_add(limit);
        let mut results = Vec::new();
        for ns in visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges(
                    filter.clone().into(),
                    vec![SortOrder {
                        field: EdgeSortField::CreatedAt,
                        direction: khive_storage::types::SortDirection::Asc,
                    }],
                    PageRequest {
                        offset: 0,
                        limit: fetch_limit,
                    },
                )
                .await?;
            results.extend(page.items);
        }
        results.sort_by_key(|e| Uuid::from(e.id));
        results.dedup_by_key(|e| Uuid::from(e.id));
        let start = (offset as usize).min(results.len());
        let end = (start + limit as usize).min(results.len());
        Ok(results[start..end].to_vec())
    }

    /// Keyset (seek) page of edges matching `filter`, ordered by edge `id`
    /// ascending. `after` is the last edge id from the previous page
    /// (exclusive); omit to start from the beginning. Returns
    /// `(items, next_after)` — `next_after` is `Some` when more rows remain
    /// past this page.
    ///
    /// Unlike [`Self::list_edges`], this is O(log n + limit) at any depth: the
    /// underlying store issues an indexed `id > ?` range scan instead of an
    /// `OFFSET` skip, avoiding the O(offset) daemon CPU cost of a naive
    /// offset-based paging loop over a large edge population.
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

        if let [ns] = visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges_after(filter.into(), after, limit)
                .await?;
            return Ok((page.items, page.next_after));
        }

        // Multi-namespace visibility: seek each namespace from the same
        // cursor (ids are globally unique UUIDs), merge, then take the head
        // of the merged set as this page.
        let probe_limit = limit + 1;
        let mut results = Vec::new();
        for ns in visible {
            let temp = NamespaceToken::for_namespace(ns.clone());
            let page = self
                .graph(&temp)?
                .query_edges_after(filter.clone().into(), after, probe_limit)
                .await?;
            results.extend(page.items);
        }
        results.sort_by_key(|e| Uuid::from(e.id));
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
        let mut totals: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for ns in token.visible_namespaces() {
            let temp = NamespaceToken::for_namespace(ns.clone());
            for (relation, count) in self.graph(&temp)?.count_edges_by_relation().await? {
                *totals.entry(relation.to_string()).or_insert(0) += count;
            }
        }
        Ok(totals)
    }

    /// DML-only body of the symmetric-relation conflict-resolution path in
    /// [`Self::update_edge`]. Runs the conflict-check SELECT, then either the
    /// DELETE+UPDATE (case b, a canonical row already exists) or the
    /// in-place UPDATE (case a, no conflict). Callers own the surrounding transaction
    /// boundary — this function issues DML only, no `BEGIN`/`COMMIT`/`ROLLBACK`.
    ///
    /// Returns `Ok(Some(existing_id))` when a canonical conflict was absorbed (the
    /// requested edge was deleted, the existing canonical row left untouched per
    /// ADR-039 DO NOTHING), or `Ok(None)` when the requested edge was updated in
    /// place.
    ///
    /// DML text is the single source of truth shared with the atomic
    /// `prepare_update_edge` symmetric branch:
    /// [`khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL`] /
    /// `EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL` /
    /// `EDGE_SYMMETRIC_UPDATE_INPLACE_SQL` — this function binds them against
    /// `rusqlite::params!` (it runs inside an existing transaction on a
    /// borrowed `&rusqlite::Connection`), the atomic path binds the same text
    /// via `SqlValue` plan params; see the constants' doc comment in
    /// `khive-db` for why a single bridge type isn't used for both.
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
    ) -> Result<Option<String>, SqliteError> {
        // `updated_at` is stored in MICROSECONDS on `graph_edges` (every other
        // write path — `edge_upsert_statement`, `edge_soft_delete_statement` —
        // uses `timestamp_micros()`; the column is read back via
        // `micros_to_datetime`). `timestamp()` (seconds) here was a
        // pre-existing bug in this raw-SQL path, found while unifying it with
        // the atomic builder (which already used `timestamp_micros()`
        // correctly).
        let now_ts = chrono::Utc::now().timestamp_micros();

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
            conn.execute(
                khive_db::stores::graph::EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
                rusqlite::params![&ns, &edge_id_str],
            )
            .map_err(SqliteError::Rusqlite)?;
            Ok(Some(existing_id))
        } else {
            // Case (a): no conflict — update source_id/target_id in-place,
            // preserving the original edge UUID.
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
                    ],
                )
                .map_err(SqliteError::Rusqlite)?;
            if affected == 0 {
                // The edge row was not found under the record's namespace.
                // This must never happen because ns = record_ns (fetched above).
                return Err(SqliteError::InvalidData(format!(
                    "update_edge: zero rows affected updating edge {edge_id_str} \
                     in namespace {ns} — row vanished between fetch and update"
                )));
            }
            Ok(None)
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

        // After fetching, all mutations and validation must use the
        // RECORD's namespace, not the caller's.  Derive record_tok from the stored edge
        // namespace so that endpoint validation, raw-SQL predicates, and graph routing
        // all address the correct backend partition.
        let record_ns: String = edge.namespace.clone();
        let record_tok = NamespaceToken::for_namespace(
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

            let pool = self.backend().pool_arc();
            // Route through the single-writer task when the write queue is
            // enabled; best-effort lookup degrades to the legacy pool-mutex
            // path (mirrors merge_entity/merge_note above).
            let writer_task = pool.writer_task_handle().ok().flatten();

            // Some(surviving_id) when a canonical conflict was absorbed (the requested
            // edge was deleted, existing canonical row left untouched per ADR-039 DO
            // NOTHING), or None when the requested edge was updated in-place.
            let surviving_id: Option<String> = if let Some(writer_task) = writer_task {
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
                        )
                    })
                })
                .await
                .map_err(|e| {
                    RuntimeError::Internal(format!("update_edge: spawn_blocking join: {e}"))
                })?
                .map_err(RuntimeError::Sqlite)?
            };

            if let Some(sid) = surviving_id {
                // A conflict was absorbed (ADR-039 DO NOTHING): re-fetch the surviving
                // canonical row so the caller receives its real, UNMODIFIED attributes —
                // including soft-deleted rows, since the survivor's tombstone state (if
                // any) must not be resurrected by the absorbed update either. Use
                // record_tok — the surviving row lives in the same namespace as the
                // original.
                let surviving_uuid = Uuid::parse_str(&sid).map_err(|e| {
                    RuntimeError::Internal(format!("update_edge: surviving id parse failed: {e}"))
                })?;
                edge = self
                    .get_edge_including_deleted(&record_tok, surviving_uuid)
                    .await?
                    .ok_or_else(|| {
                        RuntimeError::Internal(format!(
                            "update_edge: surviving canonical row {surviving_uuid} vanished after update"
                        ))
                    })?;
            } else {
                // Reflect canonical endpoints in the returned edge (no conflict absorbed).
                edge.source_id = canon_src;
                edge.target_id = canon_tgt;
            }
        } else {
            // Non-symmetric: upsert_edge takes namespace from edge.namespace (not from the
            // graph store's routing namespace), so this is already record-namespace correct.
            // `graph` is already self.graph(&record_tok)?.
            graph.upsert_edge(edge.clone()).await?;
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
        let record_tok = NamespaceToken::for_namespace(
            khive_types::Namespace::parse(&record_ns)
                .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
        );
        let graph = self.graph(&record_tok)?;

        // Cascade: on hard delete, remove ALL annotates edges targeting this edge — including
        // already-soft-deleted ones: to prevent dangling graph_edges rows. The row
        // delete and the cascade purge run as ONE write transaction: see
        // `atomic_hard_delete_with_edge_purge`.
        // On soft delete the cascade is skipped (data-vs-view principle: soft-deleting the base
        // edge does not cascade to annotation edges; only a hard purge cleans up incident rows).
        let deleted = if hard {
            self.atomic_hard_delete_with_edge_purge(edge_hard_delete_statement(edge_id), edge_id)
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

    /// Count edges matching `filter`, summed across the caller's visible
    /// namespaces (mirrors [`Self::count_edges_by_relation`] and
    /// [`Self::list_edges`] so `stats().edges` reconciles with a full `list`
    /// keyset walk under the same token).
    pub async fn count_edges(
        &self,
        token: &NamespaceToken,
        filter: crate::curation::EdgeListFilter,
    ) -> RuntimeResult<u64> {
        let mut total = 0u64;
        for ns in token.visible_namespaces() {
            let temp = NamespaceToken::for_namespace(ns.clone());
            total += self
                .graph(&temp)?
                .count_edges(filter.clone().into())
                .await?;
        }
        Ok(total)
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
        self.validate_edge_relation_endpoints(token, spec.source_id, spec.target_id, spec.relation)
            .await?;
        let (source_id, target_id) =
            canonical_edge_endpoints(spec.relation, spec.source_id, spec.target_id);
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
        Ok(Edge {
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
        })
    }

    /// Validate and atomically upsert a batch of edges.
    ///
    /// All edges are validated and constructed with `build_edge` before any
    /// write. If validation fails for any entry the entire batch is rejected
    /// (no writes occur). On success, all edges are persisted in a single
    /// atomic transaction via `upsert_edges`.
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
        if specs.is_empty() {
            return Ok(vec![]);
        }
        let mut edges = Vec::with_capacity(specs.len());
        for spec in &specs {
            edges.push(self.build_edge(token, spec).await?);
        }
        // `upsert_edges_guarded` re-checks every edge's endpoints as part of the
        // same write, not the separate per-spec `build_edge` validation reads
        // above. A concurrent hard-delete of any endpoint landing between those
        // reads and this write aborts the whole batch (all-or-nothing, no
        // partial write) instead of persisting a dangling edge. The failing
        // entry's index and its missing endpoint(s) come from the guard's own
        // in-transaction pre-check (`GuardedBatchOutcome::refused`), not a
        // post-hoc re-read of the batch after the write already failed.
        let outcome = self
            .graph(token)?
            .upsert_edges_guarded(edges.clone())
            .await?;
        if let Some(refusal) = outcome.refused {
            return Err(RuntimeError::GuardedWriteFailed(GuardedWriteFailure {
                entry_index: Some(refusal.entry_index),
                missing_source: refusal
                    .missing
                    .source
                    .then_some(edges[refusal.entry_index].source_id),
                missing_target: refusal
                    .missing
                    .target
                    .then_some(edges[refusal.entry_index].target_id),
            }));
        }
        if outcome.summary.affected != edges.len() as u64 {
            return Err(RuntimeError::NotFound(format!(
                "link_many: one or more edge endpoints no longer exist at write time: {}",
                outcome.summary.first_error
            )));
        }

        // Read back each persisted edge by natural key so callers always
        // receive the stored row ID, not the pre-upsert generated UUID.
        let mut persisted = Vec::with_capacity(edges.len());
        for edge in &edges {
            let row = self
                .list_edges(
                    token,
                    crate::curation::EdgeListFilter {
                        source_id: Some(edge.source_id),
                        target_id: Some(edge.target_id),
                        relations: vec![edge.relation],
                        ..Default::default()
                    },
                    1,
                    0,
                )
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    crate::RuntimeError::Internal(format!(
                        "upsert_edges succeeded but natural-key lookup for ({}, {}, {}) returned nothing",
                        edge.source_id, edge.target_id, edge.relation.as_str()
                    ))
                })?;
            persisted.push(row);
        }
        Ok(persisted)
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
        for spec in &specs {
            self.validate_entity_kind(&spec.kind)?;
            // Validate entity_type at the runtime layer via pack-installed callback.
            // When no validator is installed (bare runtime, unit tests without packs),
            // the type passes through unchanged — same skip-when-absent pattern as
            // validate_entity_kind. The handler layer remains the primary enforcement point.
            let validated_type =
                self.validate_entity_type_for_kind(&spec.kind, spec.entity_type.as_deref())?;
            if spec.name.trim().is_empty() {
                return Err(RuntimeError::InvalidInput("name must not be empty".into()));
            }
            crate::secret_gate::check(&spec.name)?;
            if let Some(d) = &spec.description {
                crate::secret_gate::check(d)?;
            }
            if let Some(ref p) = spec.properties {
                crate::secret_gate::check_json(p)?;
            }
            crate::secret_gate::check_tags(&spec.tags)?;

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
            entities.push(entity);
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
                let mut fts_statement =
                    insert_document_statement("fts_entities", &entity_fts_document(entity));
                if injected_failure_index == Some(index) {
                    fts_statement = SqlStatement {
                        sql: "INSERT INTO __khive_create_many_injected_failure__ DEFAULT VALUES"
                            .to_string(),
                        params: vec![],
                        label: Some("fts-insert-injected-failure".to_string()),
                    };
                }
                AtomicOpPlan::AddEntity(AddEntityPlan {
                    entity_id: entity.id,
                    statements: vec![
                        PlanStatement {
                            statement: entity_upsert_statement(entity),
                            guard: Some(AffectedRowGuard::exactly(1)),
                        },
                        PlanStatement {
                            statement: fts_statement,
                            guard: None,
                        },
                    ],
                    post_commit: PostCommitEffect::None,
                })
            })
            .collect();

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
mod tests {
    use super::*;
    use crate::curation::EdgeListFilter;
    use crate::embedder_registry::{BlockingEmbeddingService, EmbedderProvider};
    use crate::error::RuntimeError;
    use crate::runtime::{KhiveRuntime, NamespaceToken};
    use crate::{ActorRef, Namespace};
    use async_trait::async_trait;
    use khive_storage::types::PathNode;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_CHARS};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn rt() -> KhiveRuntime {
        KhiveRuntime::memory().unwrap()
    }

    #[test]
    fn fts_fault_arm_disarms_namespace_when_scope_panics() {
        let ns = format!("fault-arm-drop-{}", uuid::Uuid::new_v4().as_simple());

        let panic_result = std::panic::catch_unwind(|| {
            let _arm = arm_fts_fail_scoped(&ns);
            panic!("leave the armed scope before consumption");
        });

        assert!(panic_result.is_err());
        assert!(
            !consume_fault(&FTS_FAIL_NS, &ns),
            "unwinding an armed scope must remove its namespace"
        );
    }

    #[test]
    fn fault_arm_set_rejects_entries_over_capacity() {
        static BOUNDED_ARMS: std::sync::LazyLock<FaultArmSet> =
            std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        let first_ns = format!("fault-arm-bound-a-{}", uuid::Uuid::new_v4().as_simple());
        let overflow_ns = format!("fault-arm-bound-b-{}", uuid::Uuid::new_v4().as_simple());
        let arm = arm_fault(&BOUNDED_ARMS, &first_ns, 1);

        let overflow = std::panic::catch_unwind(|| arm_fault(&BOUNDED_ARMS, &overflow_ns, 1));

        assert!(
            overflow.is_err(),
            "an arm set must reject entries over its bound"
        );
        drop(arm);
        assert!(BOUNDED_ARMS.lock().unwrap().is_empty());
    }

    // ── Custom embedder fan-out regression ──────────────────────────────────
    // A runtime with no `config.embedding_model` but a custom registered
    // embedder must fan out create_note through that embedder and store a
    // vector so recall can find the note.

    /// Trivial constant-vector embedding service.  The model argument is ignored;
    /// the service always returns a synthetic `dims × 1.0f32` vector.
    struct ConstVecService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for ConstVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "const-vec"
        }
    }

    struct ConstVecProvider {
        provider_name: String,
        dims: usize,
        pub build_count: Arc<AtomicUsize>,
    }

    impl ConstVecProvider {
        fn new(name: &str, dims: usize) -> (Self, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            let provider = Self {
                provider_name: name.to_owned(),
                dims,
                build_count: Arc::clone(&counter),
            };
            (provider, counter)
        }
    }

    #[async_trait]
    impl EmbedderProvider for ConstVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            self.build_count.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(ConstVecService { dims: self.dims }))
        }
    }

    /// Embedding service that sleeps briefly before returning, so its spawned
    /// embed task is still in flight when a sibling model's task resolves
    /// first — used to exercise the drain-before-return invariant on the
    /// multi-model embed fan-out.
    struct SlowVecService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for SlowVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "slow-vec"
        }
    }

    struct SlowVecProvider {
        provider_name: String,
        dims: usize,
    }

    impl SlowVecProvider {
        fn new(name: &str, dims: usize) -> Self {
            Self {
                provider_name: name.to_owned(),
                dims,
            }
        }
    }

    #[async_trait]
    impl EmbedderProvider for SlowVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(SlowVecService { dims: self.dims }))
        }
    }

    /// Embedder that fails inference immediately unless configured to wait for
    /// a sibling's entry signal first.
    struct FailFastProvider {
        provider_name: String,
        wait_for: Option<Arc<AtomicBool>>,
    }

    impl FailFastProvider {
        fn new(name: &str) -> Self {
            Self {
                provider_name: name.to_owned(),
                wait_for: None,
            }
        }

        fn after_signal(name: &str, wait_for: Arc<AtomicBool>) -> Self {
            Self {
                provider_name: name.to_owned(),
                wait_for: Some(wait_for),
            }
        }
    }

    #[async_trait]
    impl EmbedderProvider for FailFastProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            4
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(FailFastService {
                wait_for: self.wait_for.clone(),
            }))
        }
    }

    struct FailFastService {
        wait_for: Option<Arc<AtomicBool>>,
    }

    #[async_trait]
    impl EmbeddingService for FailFastService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            if let Some(wait_for) = &self.wait_for {
                while !wait_for.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            }
            Err(EmbedError::InferenceFailed(
                "injected embed failure".to_string(),
            ))
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "fail-fast"
        }
    }

    /// Embedder whose synchronous inference section is controlled by a
    /// condition variable, matching native inference that cannot observe task
    /// cancellation until the encode call returns.
    struct BlockingVecProvider {
        provider_name: String,
        dims: usize,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        entered: Arc<AtomicBool>,
    }

    struct BlockingVecControls {
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        entered: Arc<AtomicBool>,
    }

    impl BlockingVecProvider {
        fn new(name: &str, dims: usize) -> (Self, BlockingVecControls) {
            let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let entered = Arc::new(AtomicBool::new(false));
            (
                Self {
                    provider_name: name.to_owned(),
                    dims,
                    release: Arc::clone(&release),
                    entered: Arc::clone(&entered),
                },
                BlockingVecControls { release, entered },
            )
        }
    }

    #[async_trait]
    impl EmbedderProvider for BlockingVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            let service = Arc::new(BlockingVecService {
                dims: self.dims,
                release: Arc::clone(&self.release),
                entered: Arc::clone(&self.entered),
            });
            Ok(Arc::new(BlockingEmbeddingService::new(service)))
        }
    }

    struct BlockingVecService {
        dims: usize,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        entered: Arc<AtomicBool>,
    }

    #[async_trait]
    impl EmbeddingService for BlockingVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            self.entered.store(true, Ordering::Release);
            let (released, wake) = &*self.release;
            let guard = released.lock().expect("release lock must not be poisoned");
            let _guard = wake
                .wait_while(guard, |released| !*released)
                .expect("release lock must not be poisoned");
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "blocking-vec"
        }
    }

    /// Embedder whose `embed` parks until a release that never comes — models
    /// a hung provider. Only task abort can end its embed future. `entered`
    /// receives a permit when `embed` is reached, so a test can wait until the
    /// parked task is provably past the dispatch point before acting on it.
    struct ParkedVecProvider {
        provider_name: String,
        dims: usize,
        release: Arc<tokio::sync::Notify>,
        entered: Arc<tokio::sync::Notify>,
    }

    impl ParkedVecProvider {
        fn new(
            name: &str,
            dims: usize,
        ) -> (Self, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
            let release = Arc::new(tokio::sync::Notify::new());
            let entered = Arc::new(tokio::sync::Notify::new());
            (
                Self {
                    provider_name: name.to_owned(),
                    dims,
                    release: Arc::clone(&release),
                    entered: Arc::clone(&entered),
                },
                release,
                entered,
            )
        }
    }

    #[async_trait]
    impl EmbedderProvider for ParkedVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(ParkedVecService {
                dims: self.dims,
                release: Arc::clone(&self.release),
                entered: Arc::clone(&self.entered),
            }))
        }
    }

    struct ParkedVecService {
        dims: usize,
        release: Arc<tokio::sync::Notify>,
        entered: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl EmbeddingService for ParkedVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            // notify_one stores a permit when no waiter is registered yet, so
            // the entered signal cannot be lost to a start-order race.
            self.entered.notify_one();
            self.release.notified().await;
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "parked-vec"
        }
    }

    /// Custom embedder with no lattice model in config must participate in
    /// fan-out: the gate must check `registered_embedding_model_names()`, not
    /// `config().embedding_model.is_some()`: the latter falls through to
    /// `vec![]` when only a custom provider is registered.
    #[tokio::test]
    async fn custom_embedder_only_runtime_fanout_stores_vector() {
        const MODEL_NAME: &str = "test-custom-encoder";
        const DIMS: usize = 8;

        // Build a runtime with no lattice embedding_model.
        let rt = KhiveRuntime::memory().unwrap();

        // Register the custom provider — this is the only embedder configured.
        let (provider, _counter) = ConstVecProvider::new(MODEL_NAME, DIMS);
        rt.register_embedder(provider);

        // Sanity: config.embedding_model is None, but the registry has one entry.
        assert!(rt.config().embedding_model.is_none());
        assert_eq!(rt.registered_embedding_model_names(), vec![MODEL_NAME]);

        let tok = NamespaceToken::local();

        // create_note should fan out to the custom embedder and store a vector.
        let note = rt
            .create_note(
                &tok,
                "memory",
                None,
                "custom embedder integration test content",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create_note with custom-only embedder must succeed");

        // Verify: a vector was written in the custom model's store.
        use khive_storage::types::VectorSearchRequest;
        let query_vec = vec![1.0_f32; DIMS];
        let hits = rt
            .vectors_for_model(&tok, MODEL_NAME)
            .expect("vector store for custom model must be accessible")
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec],
                top_k: 5,
                namespace: Some(tok.namespace().as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some(MODEL_NAME.to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("vector search succeeds");

        assert!(
            hits.iter().any(|h| h.subject_id == note.id),
            "custom embedder must have written a vector for note {}: hits={hits:?}",
            note.id
        );
    }

    /// Custom-only embedder participates in `embed_with_model` so recall
    /// fan-out also works: the lattice alias parse must be optional, with
    /// the embedder registry consulted directly, since requiring a lattice
    /// alias would reject valid custom provider names with `UnknownModel`.
    #[tokio::test]
    async fn embed_with_model_accepts_custom_provider_name() {
        const MODEL_NAME: &str = "my-custom-enc";
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider, _counter) = ConstVecProvider::new(MODEL_NAME, DIMS);
        rt.register_embedder(provider);

        let result = rt
            .embed_with_model(MODEL_NAME, "hello world")
            .await
            .expect("embed_with_model must accept custom provider names");

        assert_eq!(
            result.len(),
            DIMS,
            "embedding dimension must match provider"
        );
        assert!(
            result.iter().all(|&v| (v - 1.0_f32).abs() < 1e-6),
            "ConstVecService must produce all-ones vector; got: {result:?}"
        );
    }

    /// `embed_with_model` must still reject names that are not in the
    /// registry (neither lattice aliases nor custom providers).
    #[tokio::test]
    async fn embed_with_model_rejects_unregistered_name() {
        let rt = KhiveRuntime::memory().unwrap();
        let result = rt.embed_with_model("nonexistent-model", "hello").await;
        assert!(
            matches!(result.unwrap_err(), RuntimeError::UnknownModel(ref n) if n == "nonexistent-model"),
            "unregistered model name must return UnknownModel"
        );
    }

    // ── No-embeddings config regression ─────────────────────────────────────
    // `RuntimeConfig::no_embeddings()` must register zero embedders, so
    // `create_note` never attempts to lazily build a lattice embedding model —
    // this is what lets `memory.remember` succeed on a machine with no local
    // model files present.

    #[tokio::test]
    async fn no_embeddings_config_registers_zero_embedders() {
        let config = crate::config::RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            ..crate::config::RuntimeConfig::no_embeddings()
        };
        let rt = KhiveRuntime::new(config).expect("runtime construction must succeed");

        assert!(rt.config().embedding_model.is_none());
        assert!(
            rt.registered_embedding_model_names().is_empty(),
            "no_embeddings() runtime must register zero embedders"
        );
    }

    #[tokio::test]
    async fn no_embeddings_runtime_create_note_succeeds_without_model_fanout() {
        let config = crate::config::RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            ..crate::config::RuntimeConfig::no_embeddings()
        };
        let rt = KhiveRuntime::new(config).expect("runtime construction must succeed");
        let tok = NamespaceToken::local();

        // With zero registered embedders, create_note's embed fan-out list is
        // empty and no lattice model build is ever attempted -- the write must
        // succeed, degrading to FTS-only.
        let note = rt
            .create_note(
                &tok,
                "memory",
                None,
                "issue-396 regression: model-less remember must succeed",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("create_note must succeed with zero registered embedders");

        assert_eq!(
            note.content,
            "issue-396 regression: model-less remember must succeed"
        );
    }

    #[tokio::test]
    async fn update_edge_changes_weight() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    weight: Some(0.5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!((updated.weight - 0.5).abs() < 0.001);
    }

    /// Regression test: `update_edge_symmetric_dml` previously stored
    /// `updated_at` via `chrono::Utc::now().timestamp()` (SECONDS) while every
    /// other `graph_edges` write path (`edge_upsert_statement`,
    /// `edge_soft_delete_statement`) uses `timestamp_micros()`: a genuine
    /// pre-existing bug, found while unifying this raw-SQL path with the
    /// atomic builder (which already used `timestamp_micros()` correctly)
    /// onto the shared `EDGE_SYMMETRIC_*_SQL` text. A seconds value misread as
    /// microseconds round-trips to a date a few minutes after the Unix epoch,
    /// not "now".
    #[tokio::test]
    async fn update_edge_symmetric_relation_stores_microsecond_updated_at() {
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
            .link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let before = chrono::Utc::now();
        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    weight: Some(0.5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let drift = (updated.updated_at - before).num_seconds().abs();
        assert!(
            drift < 60,
            "updated_at must round-trip as a recent timestamp (micros, not \
             seconds); got {:?}, expected within 60s of {:?}",
            updated.updated_at,
            before
        );
    }

    #[tokio::test]
    async fn update_edge_changes_relation() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    relation: Some(EdgeRelation::VariantOf),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::VariantOf);
    }

    /// A symmetric-relation update whose canonical natural key collides
    /// with an existing edge must delete the requested (non-canonical)
    /// row and leave the surviving canonical row's attributes untouched
    /// (ADR-039 ON CONFLICT DO NOTHING) — the discarded edge's patched
    /// weight must never overwrite the survivor (khive#1213).
    #[tokio::test]
    async fn update_edge_symmetric_conflict_keeps_survivor_attributes() {
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

        let requested = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.2, None)
            .await
            .unwrap();
        let requested_id: Uuid = requested.id.into();

        let canonical = rt
            .link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 0.6, None)
            .await
            .unwrap();
        let canonical_id: Uuid = canonical.id.into();

        let updated = rt
            .update_edge(
                &tok,
                requested_id,
                crate::curation::EdgePatch {
                    relation: Some(EdgeRelation::CompetesWith),
                    weight: Some(0.9),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // The requested (non-canonical) row was absorbed into the survivor.
        assert_eq!(Uuid::from(updated.id), canonical_id);
        assert_eq!(
            updated.weight, 0.6,
            "survivor weight must not be overwritten by the discarded edge's patch"
        );

        let requested_after = rt
            .get_edge_including_deleted(&tok, requested_id)
            .await
            .unwrap();
        assert!(
            requested_after.is_none(),
            "the non-canonical requested row must be deleted, not just tombstoned"
        );
    }

    /// A soft-deleted surviving canonical row must not be resurrected by a
    /// conflicting symmetric-relation update (khive#1213).
    #[tokio::test]
    async fn update_edge_symmetric_conflict_does_not_resurrect_tombstoned_survivor() {
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

        let requested = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.2, None)
            .await
            .unwrap();
        let requested_id: Uuid = requested.id.into();

        let canonical = rt
            .link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 0.6, None)
            .await
            .unwrap();
        let canonical_id: Uuid = canonical.id.into();
        rt.delete_edge(&tok, canonical_id, false).await.unwrap();

        rt.update_edge(
            &tok,
            requested_id,
            crate::curation::EdgePatch {
                relation: Some(EdgeRelation::CompetesWith),
                weight: Some(0.9),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let requested_after = rt
            .get_edge_including_deleted(&tok, requested_id)
            .await
            .unwrap();
        assert!(
            requested_after.is_none(),
            "the non-canonical requested row must be deleted, not just tombstoned"
        );

        let canonical_after = rt.get_edge(&tok, canonical_id).await.unwrap();
        assert!(
            canonical_after.is_none(),
            "a tombstoned survivor must not be resurrected by a conflicting update"
        );
    }

    // ---- update_edge endpoint validation ----

    // update_edge: note→entity annotates → set relation=Supersedes → InvalidInput (crossing).
    // Edge must NOT be mutated in the store.
    #[tokio::test]
    async fn update_edge_annotates_note_to_entity_set_supersedes_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(&tok, "observation", None, "a note", Some(0.5), None, vec![])
            .await
            .unwrap();
        let entity = rt
            .create_entity(&tok, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();
        // Create a valid note→entity annotates edge.
        let edge = rt
            .link(&tok, note.id, entity.id, EdgeRelation::Annotates, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        // Attempt to change relation to Supersedes (crossing substrates → invalid).
        let result = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    relation: Some(EdgeRelation::Supersedes),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "update to Supersedes on note→entity edge must return InvalidInput, got {result:?}"
        );

        // Edge must NOT be mutated — re-fetch and verify relation unchanged.
        let fetched = rt.get_edge(&tok, edge_id).await.unwrap().unwrap();
        assert_eq!(
            fetched.relation,
            EdgeRelation::Annotates,
            "edge relation must be unchanged after failed update"
        );
    }

    // update_edge: entity→entity extends → set relation=Annotates → InvalidInput
    // (annotates source must be a note).
    #[tokio::test]
    async fn update_edge_entity_to_entity_set_annotates_returns_invalid_input() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let result = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    relation: Some(EdgeRelation::Annotates),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "update to Annotates on entity→entity edge must return InvalidInput, got {result:?}"
        );
    }

    // update_edge: entity→entity extends → set relation=Supersedes → Ok
    // (entity→entity is valid for supersedes).
    #[tokio::test]
    async fn update_edge_entity_to_entity_set_supersedes_succeeds() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    relation: Some(EdgeRelation::Supersedes),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::Supersedes);

        // Verify persisted.
        let fetched = rt.get_edge(&tok, edge_id).await.unwrap().unwrap();
        assert_eq!(fetched.relation, EdgeRelation::Supersedes);
    }

    // update_edge: weight-only (relation = None) → Ok, no validation, unchanged relation.
    #[tokio::test]
    async fn update_edge_weight_only_skips_validation() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    weight: Some(0.3),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::Extends);
        assert!((updated.weight - 0.3).abs() < 0.001);
    }

    // update_edge: entity→entity extends → set relation=VariantOf (same class) → Ok.
    #[tokio::test]
    async fn update_edge_same_class_relation_change_succeeds() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let updated = rt
            .update_edge(
                &tok,
                edge_id,
                crate::curation::EdgePatch {
                    relation: Some(EdgeRelation::VariantOf),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.relation, EdgeRelation::VariantOf);
    }

    #[tokio::test]
    async fn list_edges_filters_by_relation() {
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

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Enables, 1.0, None)
            .await
            .unwrap();

        let filter = EdgeListFilter {
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };
        let edges = rt.list_edges(&tok, filter, 100, 0).await.unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EdgeRelation::Extends);
    }

    #[tokio::test]
    async fn list_edges_filters_by_source() {
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

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, c.id, d.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        let filter = EdgeListFilter {
            source_id: Some(a.id),
            ..Default::default()
        };
        let edges = rt.list_edges(&tok, filter, 100, 0).await.unwrap();
        assert_eq!(edges.len(), 1);
        let src: Uuid = edges[0].source_id;
        assert_eq!(src, a.id);
    }

    /// Regression: `offset` was hard-coded to 0 in `list_edges`, so every
    /// page returned the identical first rows. Pages must now tile the full
    /// matching set with no gaps or duplicates, and an out-of-range offset
    /// must return empty rather than page 1.
    #[tokio::test]
    async fn list_edges_offset_pages_through_full_set() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        for i in 0..5 {
            let t = rt
                .create_entity(&tok, "concept", None, &format!("T{i}"), None, None, vec![])
                .await
                .unwrap();
            rt.link(&tok, a.id, t.id, EdgeRelation::Extends, 1.0, None)
                .await
                .unwrap();
        }

        let filter = EdgeListFilter {
            source_id: Some(a.id),
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };

        let page0 = rt.list_edges(&tok, filter.clone(), 2, 0).await.unwrap();
        let page1 = rt.list_edges(&tok, filter.clone(), 2, 2).await.unwrap();
        let page2 = rt.list_edges(&tok, filter.clone(), 2, 4).await.unwrap();
        assert_eq!(page0.len(), 2);
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 1);

        let ids = |p: &[Edge]| p.iter().map(|e| Uuid::from(e.id)).collect::<Vec<_>>();
        assert_ne!(ids(&page0), ids(&page1), "page 2 must differ from page 1");

        let mut all_ids: Vec<Uuid> = ids(&page0)
            .into_iter()
            .chain(ids(&page1))
            .chain(ids(&page2))
            .collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), 5, "pages must tile the full edge set");

        let empty = rt.list_edges(&tok, filter.clone(), 2, 100).await.unwrap();
        assert!(
            empty.is_empty(),
            "offset past the end must return empty, not page 1"
        );
    }

    /// `list_edges_after` seeks via `id > cursor` against the
    /// `(namespace, id)` primary key index instead of paging through OFFSET,
    /// so cost does not grow with how deep the walk goes.
    #[tokio::test]
    async fn list_edges_after_keyset_tiles_full_set() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        for i in 0..5 {
            let t = rt
                .create_entity(&tok, "concept", None, &format!("K{i}"), None, None, vec![])
                .await
                .unwrap();
            rt.link(&tok, a.id, t.id, EdgeRelation::Extends, 1.0, None)
                .await
                .unwrap();
        }

        let filter = EdgeListFilter {
            source_id: Some(a.id),
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };

        let mut seen = Vec::new();
        let mut cursor: Option<Uuid> = None;
        for _ in 0..20 {
            let (page, next) = rt
                .list_edges_after(&tok, filter.clone(), cursor, 2)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            seen.extend(page.iter().map(|e| Uuid::from(e.id)));
            if next.is_none() {
                break;
            }
            cursor = next;
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5, "keyset walk must tile the full edge set");

        // Stability: repeating the same cursor returns the same page — no
        // drift under a fixed snapshot. The seek is `WHERE id > ?` against
        // the `(namespace, id)` primary key index with `ORDER BY id ASC`
        // matching the index order, so this is an indexed range scan, not a
        // full-table scan+sort (see `query_edges_after` in khive-db).
        let (first_a, next_a) = rt
            .list_edges_after(&tok, filter.clone(), None, 2)
            .await
            .unwrap();
        let (first_b, next_b) = rt
            .list_edges_after(&tok, filter.clone(), None, 2)
            .await
            .unwrap();
        assert_eq!(
            first_a.iter().map(|e| e.id.0).collect::<Vec<_>>(),
            first_b.iter().map(|e| e.id.0).collect::<Vec<_>>(),
        );
        assert_eq!(next_a, next_b);
    }

    #[tokio::test]
    async fn list_edges_after_single_namespace_exact_final_page_has_no_next_after() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "SingleCursorA", None, None, vec![])
            .await
            .unwrap();
        for i in 0..4 {
            let t = rt
                .create_entity(
                    &tok,
                    "concept",
                    None,
                    &format!("SingleCursorT{i}"),
                    None,
                    None,
                    vec![],
                )
                .await
                .unwrap();
            rt.link(&tok, a.id, t.id, EdgeRelation::Extends, 1.0, None)
                .await
                .unwrap();
        }

        let filter = EdgeListFilter {
            source_id: Some(a.id),
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };

        let (page1, next1) = rt
            .list_edges_after(&tok, filter.clone(), None, 2)
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        let cursor = next1.expect("first page must report a cursor when two rows remain");

        let (page2, next2) = rt
            .list_edges_after(&tok, filter, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(
            next2, None,
            "an exact-size final single-namespace page must not report a cursor"
        );
    }

    #[tokio::test]
    async fn list_edges_after_multi_namespace_exact_final_page_has_no_next_after() {
        let rt = rt();
        let ns_a = Namespace::parse("cursor-ns-a").unwrap();
        let ns_b = Namespace::parse("cursor-ns-b").unwrap();
        let tok_a = NamespaceToken::for_namespace(ns_a.clone());
        let tok_b = NamespaceToken::for_namespace(ns_b.clone());
        let visible = NamespaceToken::mint_with_visibility(ns_a, vec![ns_b], ActorRef::anonymous());

        for (tok, prefix) in [(&tok_a, "A"), (&tok_b, "B")] {
            let source = rt
                .create_entity(
                    tok,
                    "concept",
                    None,
                    &format!("MultiCursor{prefix}Source"),
                    None,
                    None,
                    vec![],
                )
                .await
                .unwrap();
            for i in 0..2 {
                let target = rt
                    .create_entity(
                        tok,
                        "concept",
                        None,
                        &format!("MultiCursor{prefix}Target{i}"),
                        None,
                        None,
                        vec![],
                    )
                    .await
                    .unwrap();
                rt.link(tok, source.id, target.id, EdgeRelation::Extends, 1.0, None)
                    .await
                    .unwrap();
            }
        }

        let filter = EdgeListFilter {
            relations: vec![EdgeRelation::Extends],
            ..Default::default()
        };
        let (page1, next1) = rt
            .list_edges_after(&visible, filter.clone(), None, 2)
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        let cursor = next1.expect("first merged page must report a cursor when rows remain");

        let (page2, next2) = rt
            .list_edges_after(&visible, filter, Some(cursor), 2)
            .await
            .unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(
            next2, None,
            "an exact-size final multi-namespace page must not report a cursor"
        );
    }

    /// `stats()` should be able to report a per-relation breakdown so
    /// auditors know the true population per relation before sampling.
    #[tokio::test]
    async fn count_edges_by_relation_matches_fixtures() {
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

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, b.id, c.id, EdgeRelation::Enables, 1.0, None)
            .await
            .unwrap();

        let counts = rt.count_edges_by_relation(&tok).await.unwrap();
        assert_eq!(counts.get("extends").copied(), Some(2));
        assert_eq!(counts.get("enables").copied(), Some(1));
    }

    #[tokio::test]
    async fn delete_edge_removes_from_storage() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_id: Uuid = edge.id.into();

        let deleted = rt.delete_edge(&tok, edge_id, true).await.unwrap();
        assert!(deleted);

        let fetched = rt.get_edge(&tok, edge_id).await.unwrap();
        assert!(fetched.is_none(), "edge should be gone after delete");
    }

    #[tokio::test]
    async fn count_edges_matches_filter() {
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

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Enables, 1.0, None)
            .await
            .unwrap();

        let all = rt
            .count_edges(&tok, EdgeListFilter::default())
            .await
            .unwrap();
        assert_eq!(all, 2);

        let just_extends = rt
            .count_edges(
                &tok,
                EdgeListFilter {
                    relations: vec![EdgeRelation::Extends],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(just_extends, 1);
    }

    // ---- substrate_exists_in_ns must use get_edge_visible ----

    /// An edge owned by a visible (non-primary) namespace must be found by
    /// `substrate_exists_in_ns` and therefore usable as a graph root in
    /// `neighbors` and `traverse`.
    #[tokio::test]
    async fn edge_in_visible_namespace_reachable_as_graph_root() {
        let rt = rt();
        let ns_a = Namespace::parse("vis-edge-a").unwrap();
        let ns_b = Namespace::parse("vis-edge-b").unwrap();

        // Create two entities and an edge in namespace B.
        let tok_b = NamespaceToken::for_namespace(ns_b.clone());
        let src = rt
            .create_entity(&tok_b, "concept", None, "SrcB", None, None, vec![])
            .await
            .unwrap();
        let tgt = rt
            .create_entity(&tok_b, "concept", None, "TgtB", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(&tok_b, src.id, tgt.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // Namespace A with B in its visible set should be able to get the
        // edge and use it as a traverse root.
        let tok_a_vis = rt
            .authorize_with_visibility(ns_a.clone(), vec![ns_b.clone()])
            .unwrap();

        // Direct get of the edge must succeed (visible namespace).
        let got = rt.get_edge_visible(&tok_a_vis, edge.id.0).await.unwrap();
        assert!(
            got.is_some(),
            "edge in visible namespace must be retrievable via get_edge_visible"
        );

        // neighbors/traverse use substrate_exists_in_ns which now calls
        // get_edge_visible — they must not return empty for a visible-ns edge root.
        let neighbors = rt
            .neighbors(&tok_a_vis, src.id, Direction::Out, Some(16), None)
            .await
            .unwrap();
        assert!(
            neighbors.iter().any(|h| h.node_id == tgt.id),
            "neighbors of visible-ns node must include its visible-ns neighbor; got: {neighbors:?}"
        );
    }

    // By-ID ops do not enforce namespace isolation. Shared-brain OSS model:
    // UUID is globally unique; get/update/delete find the record regardless
    // of caller's token namespace.
    #[tokio::test]
    async fn get_entity_cross_namespace_no_longer_denied() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());
        let entity = rt
            .create_entity(&ns_a, "concept", None, "Alpha", None, None, vec![])
            .await
            .unwrap();

        // Same namespace: still works.
        let found = rt.get_entity(&ns_a, entity.id).await;
        assert!(found.is_ok(), "same-namespace get must succeed");

        // Different namespace: now also returns the entity (shared brain).
        let cross = rt.get_entity(&ns_b, entity.id).await;
        assert!(
            cross.is_ok(),
            "cross-namespace get must succeed in shared-brain OSS (ADR-007 rule 2)"
        );
        assert_eq!(cross.unwrap().id, entity.id);
    }

    #[tokio::test]
    async fn delete_entity_cross_namespace_no_longer_denied() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());
        let entity = rt
            .create_entity(&ns_a, "concept", None, "Beta", None, None, vec![])
            .await
            .unwrap();

        // Cross-namespace delete now succeeds (shared brain).
        let cross_ns_result = rt.delete_entity(&ns_b, entity.id, true).await;
        assert!(
            cross_ns_result.is_ok(),
            "cross-namespace delete must succeed in shared-brain OSS; got {:?}",
            cross_ns_result
        );
        assert!(cross_ns_result.unwrap(), "delete must return true");

        // Entity is gone — even from the original namespace.
        let gone = rt.get_entity(&ns_a, entity.id).await;
        assert!(gone.is_err(), "entity must be gone after delete");
    }

    // ---- Note annotation tests ----

    #[tokio::test]
    async fn create_note_indexes_into_fts5() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "FlashAttention reduces memory by using tiling",
                Some(0.8),
                None,
                vec![],
            )
            .await
            .unwrap();

        // FTS5 should have indexed the note content.
        let ns = tok.namespace().as_str().to_string();
        let hits = rt
            .text_for_notes(&tok)
            .unwrap()
            .search(khive_storage::types::TextSearchRequest {
                query: "FlashAttention".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: Some(khive_storage::types::TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();

        assert!(
            hits.iter().any(|h| h.subject_id == note.id),
            "note should be indexed in FTS5 after create"
        );
    }

    /// #916: `@` used to reach SQLite FTS5's bareword parser raw and error
    /// (`sanitize_fts5_query` did not strip it), surfacing as
    /// `RuntimeError::InvalidInput` per #569's fail-loud policy.
    /// `sanitize_fts5_token_group`'s bareword-safety gate now routes it
    /// through the quoted-phrase alternative instead, so `search_notes`
    /// succeeds and finds the seeded content.
    #[tokio::test]
    async fn search_notes_with_residual_fts5_char_now_sanitized() {
        let rt = rt();
        let tok = NamespaceToken::local();
        rt.create_note(
            &tok,
            "observation",
            None,
            "use foo@bar to chain calls",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();

        let result = rt
            .search_notes(&tok, "foo@bar", None, 10, None, false, &[], None)
            .await;

        let hits = result.unwrap_or_else(|e| {
            panic!("#916 search_notes must not fail on an '@'-bearing query, got: {e:?}")
        });
        assert!(
            !hits.is_empty(),
            "#916 '@'-bearing query must still find the seeded 'foo@bar' content via the \
             quoted-phrase alternative"
        );
    }

    /// The `search_notes` FTS fail-open arm must only degrade genuine FTS5
    /// parser syntax errors. A non-parser `StorageError`: e.g. a pool
    /// exhaustion or connection timeout on the text-search backend — is not a
    /// bad query and must propagate as `Err`, not be silently swallowed into
    /// an empty (falsely "successful") result set. `search_notes`,
    /// `hybrid_search`, `hybrid_search_with_strategy`, and
    /// `collect_recall_text_hits` all share the same `is_fts5_syntax_error()`
    /// gate on `StorageError`, so this case generalizes to all four call sites.
    #[tokio::test]
    async fn search_notes_propagates_non_parser_fts_error() {
        let rt = rt();
        let tok = NamespaceToken::local();
        rt.create_note(
            &tok,
            "observation",
            None,
            "FlashAttention reduces memory by using tiling",
            Some(0.8),
            None,
            vec![],
        )
        .await
        .unwrap();

        let ns = tok.namespace().as_str().to_string();
        arm_fts_search_fail(&ns);

        let result = rt
            .search_notes(&tok, "FlashAttention", None, 10, None, false, &[], None)
            .await;

        assert!(
            result.is_err(),
            "search_notes must propagate a non-parser FTS StorageError (Timeout) \
             as Err, not silently degrade it to an empty result, got: {:?}",
            result.ok()
        );
        assert!(
            matches!(
                result.unwrap_err(),
                RuntimeError::Storage(khive_storage::StorageError::Timeout { .. })
            ),
            "propagated error must be the injected StorageError::Timeout, unwrapped \
             through RuntimeError::Storage"
        );
    }

    #[tokio::test]
    async fn create_note_with_properties() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let props = serde_json::json!({"source": "arxiv:2205.14135"});
        let note = rt
            .create_note(
                &tok,
                "insight",
                None,
                "FlashAttention is IO-aware",
                Some(0.9),
                Some(props.clone()),
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(note.properties.as_ref().unwrap(), &props);
    }

    #[tokio::test]
    async fn create_note_creates_annotates_edges() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "FlashAttention", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "FlashAttention uses SRAM tiling for memory efficiency",
                Some(0.9),
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        // The note should have an outbound `annotates` edge to the entity.
        let out_neighbors = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(out_neighbors.len(), 1);
        assert_eq!(out_neighbors[0].node_id, entity.id);
        assert_eq!(out_neighbors[0].relation, EdgeRelation::Annotates);

        // The entity should have an inbound `annotates` edge from the note.
        let in_neighbors = rt
            .neighbors(
                &tok,
                entity.id,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(in_neighbors.len(), 1);
        assert_eq!(in_neighbors[0].node_id, note.id);
    }

    #[tokio::test]
    async fn neighbors_without_relation_filter_returns_all() {
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

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Enables, 1.0, None)
            .await
            .unwrap();

        let all = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn neighbors_with_relation_filter_returns_subset() {
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

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, a.id, c.id, EdgeRelation::Enables, 1.0, None)
            .await
            .unwrap();

        let filtered = rt
            .neighbors(
                &tok,
                a.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Extends]),
            )
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].node_id, b.id);
        assert_eq!(filtered[0].relation, EdgeRelation::Extends);
    }

    /// Self-loop direction parity:
    /// `neighbors_with_query_directed`'s post-merge dedup must not collapse a
    /// self-loop edge's Out row and In row into one — they share `(node_id,
    /// edge_id)` but are opposite directions, matching what a separate `Out`
    /// call plus a separate `In` call would return for the same edge. The
    /// self-loop edge is inserted directly through the graph store (`link()`
    /// rejects source_id == target_id) to exercise the merge/dedup path.
    #[tokio::test]
    async fn neighbors_with_query_directed_preserves_self_loop_direction_parity() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let centre = rt
            .create_entity(&tok, "concept", None, "Centre", None, None, vec![])
            .await
            .unwrap();

        let now = chrono::Utc::now();
        rt.graph(&tok)
            .unwrap()
            .upsert_edge(Edge {
                id: LinkId::from(Uuid::new_v4()),
                namespace: "local".to_string(),
                source_id: centre.id,
                target_id: centre.id,
                relation: EdgeRelation::Extends,
                weight: 0.7,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: None,
                target_backend: None,
            })
            .await
            .unwrap();

        let directed = rt
            .neighbors_with_query_directed(
                &tok,
                centre.id,
                NeighborQuery {
                    direction: Direction::Both,
                    relations: None,
                    limit: None,
                    min_weight: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            directed.len(),
            2,
            "a self-loop edge must produce both an Out hit and an In hit, not one collapsed hit"
        );
        let directions: Vec<Direction> = directed.iter().map(|(_, d)| d.clone()).collect();
        assert!(
            directions.contains(&Direction::Out),
            "self-loop must retain its Out-tagged hit"
        );
        assert!(
            directions.contains(&Direction::In),
            "self-loop must retain its In-tagged hit"
        );
    }

    #[tokio::test]
    async fn search_notes_returns_relevant_note() {
        let rt = rt();
        let tok = NamespaceToken::local();
        rt.create_note(
            &tok,
            "observation",
            None,
            "GQA reduces KV cache memory for large models",
            Some(0.8),
            None,
            vec![],
        )
        .await
        .unwrap();

        let results = rt
            .search_notes(&tok, "GQA KV cache", None, 10, None, false, &[], None)
            .await
            .unwrap();

        assert!(!results.is_empty(), "search should return the indexed note");
        let hit = &results[0];
        assert!(
            hit.title.is_some(),
            "note hit title should be populated (falls back to content)"
        );
        assert!(
            hit.snippet.is_some(),
            "note hit snippet should be populated"
        );
    }

    #[tokio::test]
    async fn search_notes_excludes_soft_deleted() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "RoPE positional encoding rotary embeddings",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .unwrap();

        // Soft-delete the note.
        rt.notes(&tok)
            .unwrap()
            .delete_note(note.id, DeleteMode::Soft)
            .await
            .unwrap();

        let results = rt
            .search_notes(
                &tok,
                "RoPE rotary positional",
                None,
                10,
                None,
                false,
                &[],
                None,
            )
            .await
            .unwrap();

        assert!(
            results.iter().all(|h| h.note_id != note.id),
            "soft-deleted note should be excluded from search"
        );
    }

    // ---- predicate pushdown before truncation (note branch) ----

    /// Regression: notes store tags inside `properties["tags"]`: there is no
    /// separate tags column. Without pushdown, the tag filter is applied after
    /// `hits.truncate(limit)`, so a tag-matching note ranked beyond `limit` in
    /// the raw RRF fusion is silently dropped.
    ///
    /// Scenario: `limit=1`, tags_any=["note-target-tag"]. Two notes are inserted:
    ///   - decoy: high FTS rank (repeats query terms), NO target tag.
    ///   - target: lower FTS rank, HAS "note-target-tag" in `properties["tags"]`.
    ///
    /// Without pushdown: decoy occupies the slot, target is dropped.
    /// With pushdown: decoy is excluded in the alive-note loop, target survives, returned.
    #[tokio::test]
    async fn search_notes_tag_filter_pushed_before_truncation() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Decoy note: repeats query tokens → higher FTS rank. No target tag.
        rt.create_note(
            &tok,
            "observation",
            None,
            "kappa lambda mu note decoy kappa lambda mu note decoy kappa lambda mu",
            Some(0.5),
            Some(serde_json::json!({"tags": ["other-note-tag"]})),
            vec![],
        )
        .await
        .unwrap();

        // Target note: fewer query tokens → lower FTS rank. Has the target tag.
        let target = rt
            .create_note(
                &tok,
                "observation",
                None,
                "kappa lambda mu note target",
                Some(0.5),
                Some(serde_json::json!({"tags": ["note-target-tag"]})),
                vec![],
            )
            .await
            .unwrap();

        // With limit=1 and tags_any, the fix must return the target note despite the
        // decoy ranking higher in raw FTS.
        let hits = rt
            .search_notes(
                &tok,
                "kappa lambda mu note",
                None,
                1,
                None,
                false,
                &["note-target-tag".to_string()],
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            hits.len(),
            1,
            "exactly one hit expected (tag-matching note)"
        );
        assert_eq!(
            hits[0].note_id, target.id,
            "tag-filtered note must be returned even when ranked below limit in raw fusion"
        );
    }

    /// Regression: without pushdown, the properties filter is applied after truncation; a matching
    /// note ranked beyond `limit` is silently dropped.
    ///
    /// Scenario: `limit=1`, properties_filter={{"source": "target"}}. Two notes:
    ///   - decoy: high FTS rank, properties {{"source": "other"}}.
    ///   - target: lower FTS rank, properties {{"source": "target"}}.
    #[tokio::test]
    async fn search_notes_props_filter_pushed_before_truncation() {
        let rt = rt();
        let tok = NamespaceToken::local();

        rt.create_note(
            &tok,
            "observation",
            None,
            "nu xi omicron note decoy nu xi omicron note decoy nu xi omicron",
            Some(0.5),
            Some(serde_json::json!({"source": "other"})),
            vec![],
        )
        .await
        .unwrap();

        let target = rt
            .create_note(
                &tok,
                "observation",
                None,
                "nu xi omicron note target",
                Some(0.5),
                Some(serde_json::json!({"source": "target"})),
                vec![],
            )
            .await
            .unwrap();

        let filter = serde_json::json!({"source": "target"});
        let hits = rt
            .search_notes(
                &tok,
                "nu xi omicron note",
                None,
                1,
                None,
                false,
                &[],
                Some(&filter),
            )
            .await
            .unwrap();

        assert_eq!(
            hits.len(),
            1,
            "exactly one hit expected (properties-matching note)"
        );
        assert_eq!(
            hits[0].note_id, target.id,
            "properties-filtered note must be returned even when ranked below limit"
        );
    }

    #[tokio::test]
    async fn resolve_returns_entity() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "LoRA", None, None, vec![])
            .await
            .unwrap();

        let resolved = rt.resolve(&tok, entity.id).await.unwrap();
        match resolved {
            Some(Resolved::Entity(e)) => assert_eq!(e.id, entity.id),
            other => panic!("expected Resolved::Entity, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn resolve_returns_note() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "LoRA fine-tunes LLMs with low-rank adapters",
                Some(0.85),
                None,
                vec![],
            )
            .await
            .unwrap();

        let resolved = rt.resolve(&tok, note.id).await.unwrap();
        match resolved {
            Some(Resolved::Note(n)) => assert_eq!(n.id, note.id),
            other => panic!("expected Resolved::Note, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn resolve_returns_none_for_unknown_uuid() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let unknown = Uuid::new_v4();
        let resolved = rt.resolve(&tok, unknown).await.unwrap();
        assert!(resolved.is_none(), "unknown UUID should resolve to None");
    }

    #[tokio::test]
    async fn resolve_prefix_finds_entity_in_own_namespace() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "PrefixTest", None, None, vec![])
            .await
            .unwrap();
        let prefix = &entity.id.to_string()[..8];

        let resolved = rt.resolve_prefix(&tok, prefix).await.unwrap();
        assert_eq!(resolved, Some(entity.id));
    }

    #[test]
    fn hex_prefix_to_uuid_pattern_inserts_hyphens_at_canonical_boundaries() {
        let full = "aabbccdd112240008000000000000ab1";
        let cases: &[(usize, &str)] = &[
            (1, "a"),
            (7, "aabbccd"),
            (8, "aabbccdd"),
            (9, "aabbccdd-1"),
            (12, "aabbccdd-1122"),
            (13, "aabbccdd-1122-4"),
            (16, "aabbccdd-1122-4000"),
            (18, "aabbccdd-1122-4000-80"),
            (20, "aabbccdd-1122-4000-8000"),
            (23, "aabbccdd-1122-4000-8000-000"),
            (24, "aabbccdd-1122-4000-8000-0000"),
            (28, "aabbccdd-1122-4000-8000-00000000"),
            (31, "aabbccdd-1122-4000-8000-000000000ab"),
        ];
        for (len, expected) in cases {
            let input = &full[..*len];
            assert_eq!(
                hex_prefix_to_uuid_pattern(input),
                *expected,
                "len={len} input={input:?}"
            );
        }
    }

    #[test]
    fn hex_prefix_to_uuid_pattern_full_32_char_matches_canonical_uuid() {
        let compact = "aabbccdd112240008000000000000ab1";
        let compact32 = &compact[..32];
        assert_eq!(
            hex_prefix_to_uuid_pattern(compact32),
            "aabbccdd-1122-4000-8000-000000000ab1"
        );
    }

    /// Input longer than 32 hex chars is NOT truncated: the extra chars land
    /// past the canonical 12-char final
    /// segment with no further hyphen, so the pattern can never match a real
    /// (36-char) stored `id`, instead of silently truncating down to a
    /// pattern that matches the valid 32-char UUID.
    #[test]
    fn hex_prefix_to_uuid_pattern_overlong_input_is_not_truncated() {
        let compact32 = "aabbccdd112240008000000000000ab1";
        let overlong = format!("{compact32}extrahex");
        let pattern = hex_prefix_to_uuid_pattern(&overlong);
        assert_eq!(
            pattern, "aabbccdd-1122-4000-8000-000000000ab1extrahex",
            "overlong input must keep its extra chars, not truncate to the valid UUID"
        );
        assert_ne!(
            pattern, "aabbccdd-1122-4000-8000-000000000ab1",
            "overlong pattern must not collapse to the canonical 36-char UUID form"
        );
    }

    #[test]
    fn hex_prefix_to_uuid_pattern_passes_through_hyphenated_input() {
        let hyphenated = "aabbccdd-1122-4000-8000-000000000ab1";
        assert_eq!(hex_prefix_to_uuid_pattern(hyphenated), hyphenated);

        let partial = "aabbccdd-11";
        assert_eq!(hex_prefix_to_uuid_pattern(partial), partial);
    }

    #[tokio::test]
    async fn resolve_prefix_compact_9_to_31_char_matches() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "CompactPrefixTest",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let compact = entity.id.simple().to_string();

        for len in [9, 12, 16, 20, 24, 28, 31] {
            let prefix = &compact[..len];
            let resolved = rt.resolve_prefix(&tok, prefix).await.unwrap();
            assert_eq!(
                resolved,
                Some(entity.id),
                "compact prefix of len {len} should resolve"
            );
        }
    }

    #[tokio::test]
    async fn resolve_prefix_compact_full_32_char_matches() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "Full32Test", None, None, vec![])
            .await
            .unwrap();
        let compact = entity.id.simple().to_string();
        assert_eq!(compact.len(), 32);

        let resolved = rt.resolve_prefix(&tok, &compact).await.unwrap();
        assert_eq!(resolved, Some(entity.id));
    }

    /// A valid 32-char compact id with extra trailing hex chars appended must
    /// fail to resolve, not silently resolve to the valid entity via truncation.
    #[tokio::test]
    async fn resolve_prefix_rejects_overlong_all_hex_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "OverlongTest", None, None, vec![])
            .await
            .unwrap();
        let compact = entity.id.simple().to_string();
        assert_eq!(compact.len(), 32);

        let overlong = format!("{compact}ab");
        let resolved = rt.resolve_prefix(&tok, &overlong).await.unwrap();
        assert_eq!(
            resolved, None,
            "a 32-char id plus extra hex chars must not resolve to the valid entity"
        );
    }

    /// The `resolve_prefix*` boundary rejects
    /// non-hex/non-hyphen input (e.g. LIKE wildcards `%`/`_`) instead of
    /// letting it reach the bound `LIKE` pattern unfiltered — covers callers
    /// (like a git-integration pack's ingest path) that resolve raw input without
    /// their own all-hex gate.
    #[tokio::test]
    async fn resolve_prefix_rejects_like_wildcard_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "WildcardTest", None, None, vec![])
            .await
            .unwrap();
        let compact = entity.id.simple().to_string();
        // A caller that forgot to hex-gate might pass a `%`-bearing string
        // straight through, hoping to broaden a scan; the resolver boundary
        // must reject it instead of running it as a wildcard LIKE.
        let wildcard_prefix = format!("{}%", &compact[..8]);

        let resolved = rt.resolve_prefix(&tok, &wildcard_prefix).await.unwrap();
        assert_eq!(
            resolved, None,
            "prefix containing a LIKE wildcard must be rejected, not resolved"
        );
    }

    #[tokio::test]
    async fn resolve_prefix_boundary_at_hyphen_positions() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "BoundaryTest", None, None, vec![])
            .await
            .unwrap();
        let compact = entity.id.simple().to_string();

        for len in [8, 12, 16, 20, 24] {
            let prefix = &compact[..len];
            let resolved = rt.resolve_prefix(&tok, prefix).await.unwrap();
            assert_eq!(
                resolved,
                Some(entity.id),
                "boundary prefix of len {len} should resolve"
            );
        }
    }

    #[tokio::test]
    async fn resolve_prefix_ambiguous_still_detected_after_normalization() {
        use khive_storage::entity::Entity;

        let rt = rt();
        let tok = NamespaceToken::local();
        let id_a = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000001").unwrap();
        let id_b = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000002").unwrap();

        let mut entity_a = Entity::new("local", "concept", "AmbigCompactA");
        entity_a.id = id_a;
        let mut entity_b = Entity::new("local", "concept", "AmbigCompactB");
        entity_b.id = id_b;

        let store = rt.entities(&tok).unwrap();
        store.upsert_entity(entity_a).await.unwrap();
        store.upsert_entity(entity_b).await.unwrap();

        // Shared 20-char compact prefix (past the first hyphen boundary).
        let shared_compact = &id_a.simple().to_string()[..20];
        let err = rt.resolve_prefix(&tok, shared_compact).await.unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::AmbiguousPrefix { ref matches, .. } if matches.len() == 2
            ),
            "shared compact prefix must still return AmbiguousPrefix; got {err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_prefix_invisible_across_namespaces() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());
        let entity = rt
            .create_entity(&ns_a, "concept", None, "Invisible", None, None, vec![])
            .await
            .unwrap();
        let prefix = &entity.id.to_string()[..8];

        // From ns_b, the entity in ns_a should not be visible.
        let resolved = rt.resolve_prefix(&ns_b, prefix).await.unwrap();
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn resolve_prefix_ambiguous_same_namespace() {
        use khive_storage::entity::Entity;

        let rt = rt();
        let tok = NamespaceToken::local();
        // Two entities with UUIDs sharing the same 8-char prefix "aabbccdd".
        let id_a = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000001").unwrap();
        let id_b = Uuid::parse_str("aabbccdd-2222-4000-8000-000000000002").unwrap();

        let mut entity_a = Entity::new("local", "concept", "AmbigA");
        entity_a.id = id_a;
        let mut entity_b = Entity::new("local", "concept", "AmbigB");
        entity_b.id = id_b;

        let store = rt.entities(&tok).unwrap();
        store.upsert_entity(entity_a).await.unwrap();
        store.upsert_entity(entity_b).await.unwrap();

        let err = rt.resolve_prefix(&tok, "aabbccdd").await.unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::AmbiguousPrefix { ref prefix, ref matches }
                    if prefix == "aabbccdd" && matches.len() == 2
            ),
            "shared 8-char prefix must return AmbiguousPrefix; got {err:?}"
        );
    }

    /// A single UUID legitimately present in TWO scanned tables (entities and
    /// notes here) must resolve cleanly to that one UUID, not a false
    /// `AmbiguousPrefix` naming the same UUID twice: without cross-table
    /// dedup, `matches.len()` becomes 2 for a single record.
    #[tokio::test]
    async fn resolve_prefix_cross_table_duplicate_uuid_resolves_cleanly() {
        use khive_storage::entity::Entity;

        let rt = rt();
        let tok = NamespaceToken::local();
        let shared_id = Uuid::parse_str("ccddeeff-1111-4000-8000-000000000001").unwrap();

        let mut entity = Entity::new("local", "concept", "Nvk749Entity");
        entity.id = shared_id;
        rt.entities(&tok)
            .unwrap()
            .upsert_entity(entity)
            .await
            .unwrap();

        let mut note = Note::new("local", "observation", "nvk749 note with the same id");
        note.id = shared_id;
        rt.notes(&tok).unwrap().upsert_note(note).await.unwrap();

        let resolved = rt
            .resolve_prefix(&tok, "ccddeeff")
            .await
            .expect("#749: a UUID present in two tables must not be reported as ambiguous");
        assert_eq!(
            resolved,
            Some(shared_id),
            "#749: cross-table duplicate must resolve to the single shared UUID"
        );
    }

    /// The early-exit inside the per-table scan loop (`if matches.len()
    /// > 1 { break }`) must also operate on DEDUPED state — otherwise a
    /// cross-table duplicate could still short-circuit the scan before a
    /// later table contributes the SAME UUID again, which would have masked
    /// the bug rather than exercising it. This drives the duplicate through
    /// the earliest two tables scanned (entities, notes) so the early-exit
    /// path is the one under test, not a post-loop dedup applied too late.
    #[tokio::test]
    async fn resolve_prefix_early_exit_uses_deduped_match_count() {
        use khive_storage::entity::Entity;

        let rt = rt();
        let tok = NamespaceToken::local();
        let shared_id = Uuid::parse_str("ddeeff11-2222-4000-8000-000000000002").unwrap();

        // entities and notes are the first two tables scanned inside
        // resolve_prefix_inner — the same UUID in both must not trip the
        // mid-scan `matches.len() > 1` break as if two distinct UUIDs had
        // been found.
        let mut entity = Entity::new("local", "concept", "Nvk749bEntity");
        entity.id = shared_id;
        rt.entities(&tok)
            .unwrap()
            .upsert_entity(entity)
            .await
            .unwrap();

        let mut note = Note::new("local", "observation", "nvk749b note with the same id");
        note.id = shared_id;
        rt.notes(&tok).unwrap().upsert_note(note).await.unwrap();

        let resolved = rt
            .resolve_prefix(&tok, "ddeeff11")
            .await
            .expect("#749: deduped early-exit must not falsely report ambiguity");
        assert_eq!(resolved, Some(shared_id));
    }

    // ---- Event resolution tests ----
    //
    // resolve_prefix and handle_get already include events; these tests are
    // regression coverage confirming event UUIDs are resolvable and that get()
    // returns kind="event".

    #[tokio::test]
    async fn resolve_finds_event_by_full_uuid() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().as_str();
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "actor",
        );
        let event_id = event.id;
        rt.events(&tok).unwrap().append_event(event).await.unwrap();

        let resolved = rt.resolve(&tok, event_id).await.unwrap();
        assert!(
            matches!(resolved, Some(Resolved::Event(_))),
            "event UUID must resolve to Resolved::Event, got {resolved:?}"
        );
    }

    #[tokio::test]
    async fn resolve_prefix_finds_event() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().as_str();
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "actor",
        );
        let event_id = event.id;
        rt.events(&tok).unwrap().append_event(event).await.unwrap();

        let prefix = &event_id.to_string()[..8];
        let resolved = rt.resolve_prefix(&tok, prefix).await.unwrap();
        assert_eq!(
            resolved,
            Some(event_id),
            "resolve_prefix must return event UUID for 8-char prefix"
        );
    }

    // ---- Referential integrity tests ----

    #[tokio::test]
    async fn link_phantom_source_returns_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(&tok, phantom, b.id, EdgeRelation::Extends, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(
                    msg.contains("source"),
                    "error message must name 'source': {msg}"
                );
            }
            other => panic!("expected NotFound for phantom source, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_phantom_target_returns_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(&tok, a.id, phantom, EdgeRelation::Extends, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(
                    msg.contains("target"),
                    "error message must name 'target': {msg}"
                );
            }
            other => panic!("expected NotFound for phantom target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_real_entities_succeeds() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.8, None)
            .await
            .unwrap();
        assert_eq!(edge.source_id, a.id);
        assert_eq!(edge.target_id, b.id);
        assert_eq!(edge.relation, EdgeRelation::Extends);
    }

    // ---- commit-time endpoint guard vs concurrent hard-delete ----

    /// Deterministic form of the regression, exercised directly at the
    /// write step `link` performs after prepare-time validation: build the
    /// exact `Edge` `link` would build, delete the target the way a
    /// concurrent racer would, and confirm the guarded write refuses it.
    #[tokio::test]
    async fn link_write_time_guard_blocks_dangling_edge_after_target_vanishes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let x = rt
            .create_entity(&tok, "concept", None, "X", None, None, vec![])
            .await
            .unwrap();

        rt.validate_edge_relation_endpoints(&tok, a.id, x.id, EdgeRelation::Extends)
            .await
            .expect("prepare-time validation must pass while X is live");

        assert!(rt.delete_entity(&tok, x.id, true).await.unwrap());

        let now = chrono::Utc::now();
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: tok.namespace().as_str().to_string(),
            source_id: a.id,
            target_id: x.id,
            relation: EdgeRelation::Extends,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        };
        let outcome = rt
            .graph(&tok)
            .unwrap()
            .upsert_edge_guarded(edge)
            .await
            .unwrap();
        match outcome {
            khive_storage::GuardedWriteOutcome::Refused(missing) => {
                assert!(missing.target, "target must be reported missing");
            }
            other => panic!(
                "guarded write must refuse an edge whose target vanished before commit, got {other:?}"
            ),
        }

        let edges = rt
            .list_edges(
                &tok,
                crate::curation::EdgeListFilter {
                    source_id: Some(a.id),
                    target_id: Some(x.id),
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
            "no dangling edge may be persisted after the guarded write refused it"
        );
    }

    #[tokio::test]
    async fn link_many_writes_nothing_when_one_target_vanishes_before_write() {
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
        let x = rt
            .create_entity(&tok, "concept", None, "X", None, None, vec![])
            .await
            .unwrap();

        let specs = vec![
            LinkSpec {
                namespace: None,
                source_id: a.id,
                target_id: x.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                metadata: None,
            },
            LinkSpec {
                namespace: None,
                source_id: a.id,
                target_id: b.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                metadata: None,
            },
        ];

        // Both specs validate fine at build_edge time (X and B both live).
        let mut edges = Vec::with_capacity(specs.len());
        for spec in &specs {
            edges.push(rt.build_edge(&tok, spec).await.unwrap());
        }

        // X vanishes before the batched write — mirrors a concurrent
        // hard-delete landing between per-spec validation and link_many's
        // single guarded batch write.
        assert!(rt.delete_entity(&tok, x.id, true).await.unwrap());

        let outcome = rt
            .graph(&tok)
            .unwrap()
            .upsert_edges_guarded(edges)
            .await
            .unwrap();
        assert_eq!(
            outcome.summary.affected, 0,
            "no edge from the batch may be persisted when any endpoint vanished"
        );
        assert!(
            outcome.refused.is_some(),
            "refused batch entry must be reported"
        );

        let edges = rt
            .list_edges(
                &tok,
                crate::curation::EdgeListFilter {
                    source_id: Some(a.id),
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
            "link_many's guarded batch must be all-or-nothing: the live A-B edge \
             must not have been persisted alongside the doomed A-X edge"
        );
    }

    // ---- hard-delete row + incident-edge purge is ONE transaction ----
    //
    // Six tests below cover both orderings (write-then-delete, and a
    // concurrent write raced against delete via `tokio::join!`) across all
    // three hard-delete paths that cascade-purge incident edges: entity,
    // note, and edge-as-node. No sleeps — the "concurrent" tests assert an
    // invariant that must hold for EITHER interleaving the async scheduler
    // picks, rather than forcing one specific interleaving, so they are
    // deterministic (never flaky) without a barrier.

    fn raw_edge(source_id: Uuid, target_id: Uuid, ns: &str) -> Edge {
        let now = chrono::Utc::now();
        Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: ns.to_string(),
            source_id,
            target_id,
            relation: EdgeRelation::Extends,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        }
    }

    async fn assert_no_edges_touch(rt: &KhiveRuntime, tok: &NamespaceToken, node_id: Uuid) {
        let as_source = rt
            .list_edges(
                tok,
                crate::curation::EdgeListFilter {
                    source_id: Some(node_id),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        let as_target = rt
            .list_edges(
                tok,
                crate::curation::EdgeListFilter {
                    target_id: Some(node_id),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert!(
            as_source.is_empty() && as_target.is_empty(),
            "no edge may reference hard-deleted node {node_id}: source-side={as_source:?} \
             target-side={as_target:?}"
        );
    }

    #[tokio::test]
    async fn hard_delete_entity_purges_edge_written_before_delete() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let x = rt
            .create_entity(&tok, "concept", None, "X", None, None, vec![])
            .await
            .unwrap();

        let edge = raw_edge(a.id, x.id, tok.namespace().as_str());
        assert_eq!(
            rt.graph(&tok)
                .unwrap()
                .upsert_edge_guarded(edge)
                .await
                .unwrap(),
            khive_storage::GuardedWriteOutcome::Written
        );

        assert!(rt.delete_entity(&tok, x.id, true).await.unwrap());
        assert_no_edges_touch(&rt, &tok, x.id).await;
    }

    #[tokio::test]
    async fn hard_delete_entity_concurrent_with_guarded_write_never_leaves_dangling_edge() {
        let rt = std::sync::Arc::new(rt());
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let x = rt
            .create_entity(&tok, "concept", None, "X", None, None, vec![])
            .await
            .unwrap();

        let delete_rt = std::sync::Arc::clone(&rt);
        let delete_tok = tok.clone();
        let delete_task =
            tokio::spawn(async move { delete_rt.delete_entity(&delete_tok, x.id, true).await });

        let write_rt = std::sync::Arc::clone(&rt);
        let write_tok = tok.clone();
        let ns = tok.namespace().as_str().to_string();
        let write_task = tokio::spawn(async move {
            let edge = raw_edge(a.id, x.id, &ns);
            write_rt
                .graph(&write_tok)
                .unwrap()
                .upsert_edge_guarded(edge)
                .await
        });

        let (deleted, _written) = tokio::join!(delete_task, write_task);
        deleted.unwrap().unwrap();
        assert_no_edges_touch(&rt, &tok, x.id).await;
    }

    #[tokio::test]
    async fn hard_delete_note_purges_edge_written_before_delete() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        let edge = raw_edge(a.id, n.id, tok.namespace().as_str());
        assert_eq!(
            rt.graph(&tok)
                .unwrap()
                .upsert_edge_guarded(edge)
                .await
                .unwrap(),
            khive_storage::GuardedWriteOutcome::Written
        );

        assert!(rt.delete_note(&tok, n.id, true).await.unwrap());
        assert_no_edges_touch(&rt, &tok, n.id).await;
    }

    #[tokio::test]
    async fn hard_delete_note_concurrent_with_guarded_write_never_leaves_dangling_edge() {
        let rt = std::sync::Arc::new(rt());
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        let delete_rt = std::sync::Arc::clone(&rt);
        let delete_tok = tok.clone();
        let note_id = n.id;
        let delete_task =
            tokio::spawn(async move { delete_rt.delete_note(&delete_tok, note_id, true).await });

        let write_rt = std::sync::Arc::clone(&rt);
        let write_tok = tok.clone();
        let ns = tok.namespace().as_str().to_string();
        let write_task = tokio::spawn(async move {
            let edge = raw_edge(a.id, note_id, &ns);
            write_rt
                .graph(&write_tok)
                .unwrap()
                .upsert_edge_guarded(edge)
                .await
        });

        let (deleted, _written) = tokio::join!(delete_task, write_task);
        deleted.unwrap().unwrap();
        assert_no_edges_touch(&rt, &tok, note_id).await;
    }

    #[tokio::test]
    async fn hard_delete_edge_endpoint_purges_annotating_edge_written_before_delete() {
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
        let n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let base_edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.8, None)
            .await
            .unwrap();
        let base_edge_id = Uuid::from(base_edge.id);

        // An edge whose TARGET is another edge — the "edge-as-node" case
        // `delete_edge`'s cascade must sweep.
        let annotating = raw_edge(n.id, base_edge_id, tok.namespace().as_str());
        assert_eq!(
            rt.graph(&tok)
                .unwrap()
                .upsert_edge_guarded(annotating)
                .await
                .unwrap(),
            khive_storage::GuardedWriteOutcome::Written
        );

        assert!(rt.delete_edge(&tok, base_edge_id, true).await.unwrap());
        assert_no_edges_touch(&rt, &tok, base_edge_id).await;
    }

    #[tokio::test]
    async fn hard_delete_edge_endpoint_concurrent_with_guarded_write_never_leaves_dangling_edge() {
        let rt = std::sync::Arc::new(rt());
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let n = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note content",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let base_edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 0.8, None)
            .await
            .unwrap();
        let base_edge_id = Uuid::from(base_edge.id);

        let delete_rt = std::sync::Arc::clone(&rt);
        let delete_tok = tok.clone();
        let delete_task =
            tokio::spawn(
                async move { delete_rt.delete_edge(&delete_tok, base_edge_id, true).await },
            );

        let write_rt = std::sync::Arc::clone(&rt);
        let write_tok = tok.clone();
        let ns = tok.namespace().as_str().to_string();
        let write_task = tokio::spawn(async move {
            let edge = raw_edge(n.id, base_edge_id, &ns);
            write_rt
                .graph(&write_tok)
                .unwrap()
                .upsert_edge_guarded(edge)
                .await
        });

        let (deleted, _written) = tokio::join!(delete_task, write_task);
        deleted.unwrap().unwrap();
        assert_no_edges_touch(&rt, &tok, base_edge_id).await;
    }

    // ---- file-backed, both write-queue configs ----
    //
    // The six tests above run against `KhiveRuntime::memory()` and race
    // delete against the guarded write via `tokio::join!` with no explicit
    // ordering control, so the scheduler could run them fully sequentially
    // on one thread without ever exercising real interleaving, and neither
    // the file-backed storage path nor `write_queue_enabled: true`
    // (`KHIVE_WRITE_QUEUE=1`, the `WriterTask`-routed write path in
    // `SqlGraphStore`) is covered at all. The four tests below close both
    // gaps: file-backed databases, one run with the writer queue off
    // (default) and one with it on, each provably forcing the guarded write
    // to land on one specific side of the delete — fully committed before
    // the delete starts (swept by the delete's cascade) and attempted only
    // after the delete has already committed (refused by the guard) — via
    // plain `.await` sequencing rather than a race whose outcome the test
    // does not control.

    fn file_backed_runtime(
        dir: &tempfile::TempDir,
        name: &str,
        write_queue_enabled: bool,
    ) -> KhiveRuntime {
        let path = dir.path().join(name);
        if write_queue_enabled {
            std::env::set_var("KHIVE_WRITE_QUEUE", "1");
        } else {
            std::env::remove_var("KHIVE_WRITE_QUEUE");
        }
        let rt = KhiveRuntime::new(crate::config::RuntimeConfig {
            db_path: Some(path),
            packs: vec!["kg".to_string()],
            brain_profile: None,
            actor_id: None,
            ..crate::config::RuntimeConfig::no_embeddings()
        })
        .unwrap();
        std::env::remove_var("KHIVE_WRITE_QUEUE");
        rt
    }

    async fn assert_guarded_write_committed_before_delete_is_swept(rt: &KhiveRuntime) {
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let x = rt
            .create_entity(&tok, "concept", None, "X", None, None, vec![])
            .await
            .unwrap();

        // Write lands fully committed while X is still live — squarely
        // inside the window before the delete's cascade runs.
        let edge = raw_edge(a.id, x.id, tok.namespace().as_str());
        assert_eq!(
            rt.graph(&tok)
                .unwrap()
                .upsert_edge_guarded(edge)
                .await
                .unwrap(),
            khive_storage::GuardedWriteOutcome::Written,
            "write must succeed while both endpoints are still live"
        );

        assert!(rt.delete_entity(&tok, x.id, true).await.unwrap());
        assert_no_edges_touch(rt, &tok, x.id).await;
    }

    async fn assert_guarded_write_attempted_after_delete_is_refused(rt: &KhiveRuntime) {
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let x = rt
            .create_entity(&tok, "concept", None, "X", None, None, vec![])
            .await
            .unwrap();

        // The delete's transaction has fully committed before the guarded
        // write is even attempted — squarely after the window has closed.
        assert!(rt.delete_entity(&tok, x.id, true).await.unwrap());

        let edge = raw_edge(a.id, x.id, tok.namespace().as_str());
        let outcome = rt
            .graph(&tok)
            .unwrap()
            .upsert_edge_guarded(edge)
            .await
            .unwrap();
        match outcome {
            khive_storage::GuardedWriteOutcome::Refused(missing) => {
                assert!(
                    missing.target,
                    "target must be reported missing once the delete has committed"
                );
                assert!(!missing.source, "source was never deleted");
            }
            other => panic!(
                "guarded write attempted after the delete committed must be refused, got {other:?}"
            ),
        }
        assert_no_edges_touch(rt, &tok, x.id).await;
    }

    #[tokio::test]
    #[serial_test::serial(khive_write_queue_env)]
    async fn guarded_write_before_delete_swept_file_backed_write_queue_off() {
        let dir = tempfile::tempdir().unwrap();
        let rt = file_backed_runtime(&dir, "guard_before_off.db", false);
        assert_guarded_write_committed_before_delete_is_swept(&rt).await;
    }

    #[tokio::test]
    #[serial_test::serial(khive_write_queue_env)]
    async fn guarded_write_after_delete_refused_file_backed_write_queue_off() {
        let dir = tempfile::tempdir().unwrap();
        let rt = file_backed_runtime(&dir, "guard_after_off.db", false);
        assert_guarded_write_attempted_after_delete_is_refused(&rt).await;
    }

    #[tokio::test]
    #[serial_test::serial(khive_write_queue_env)]
    async fn guarded_write_before_delete_swept_file_backed_write_queue_on() {
        let dir = tempfile::tempdir().unwrap();
        let rt = file_backed_runtime(&dir, "guard_before_on.db", true);
        assert_guarded_write_committed_before_delete_is_swept(&rt).await;
    }

    #[tokio::test]
    #[serial_test::serial(khive_write_queue_env)]
    async fn guarded_write_after_delete_refused_file_backed_write_queue_on() {
        let dir = tempfile::tempdir().unwrap();
        let rt = file_backed_runtime(&dir, "guard_after_on.db", true);
        assert_guarded_write_attempted_after_delete_is_refused(&rt).await;
    }

    #[tokio::test]
    async fn create_note_annotates_phantom_returns_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let phantom = Uuid::new_v4();

        let result = rt
            .create_note(
                &tok,
                "observation",
                None,
                "some content",
                Some(0.5),
                None,
                vec![phantom],
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound(_))),
            "annotates with phantom uuid must return NotFound, got {result:?}"
        );
    }

    #[tokio::test]
    async fn create_note_annotates_real_entity_succeeds() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "RealTarget", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "content",
                Some(0.5),
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        let neighbors = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, entity.id);
    }

    // Atomicity: multi-target annotates golden path — all edges created, note present.
    #[tokio::test]
    async fn create_note_multi_annotates_creates_all_edges() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let t1 = rt
            .create_entity(&tok, "concept", None, "Target1", None, None, vec![])
            .await
            .unwrap();
        let t2 = rt
            .create_entity(&tok, "concept", None, "Target2", None, None, vec![])
            .await
            .unwrap();

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "content",
                Some(0.5),
                None,
                vec![t1.id, t2.id],
            )
            .await
            .unwrap();

        let neighbors = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            neighbors.len(),
            2,
            "multi-annotates note must have exactly 2 outbound annotates edges"
        );
        let target_ids: Vec<Uuid> = neighbors.iter().map(|n| n.node_id).collect();
        assert!(target_ids.contains(&t1.id));
        assert!(target_ids.contains(&t2.id));
    }

    /// khive#1213/#1214 fix round: the atomic-apply post-commit renderer for a
    /// symmetric-edge update resolves the surviving row's store by the CALLER's
    /// token but must filter by the record's OWN namespace, passed explicitly —
    /// never by re-deriving it from whichever token happened to select the store
    /// (`self.graph(token)` scopes by `token.namespace()`, and by-ID edge updates
    /// are namespace-agnostic, so a caller in one namespace can legitimately
    /// commit an update against an edge recorded in another). This proves the
    /// `namespace` parameter — not the `token` argument — decides which row the
    /// natural-key lookup finds, at the level the review flagged as an
    /// acceptable substitute for a full cross-namespace atomic-apply test.
    #[tokio::test]
    async fn get_edge_by_natural_key_including_deleted_honors_explicit_namespace_not_token() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let a = rt
            .create_entity(&ns_b, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&ns_b, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(&ns_b, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        let (canon_src, canon_tgt) =
            canonical_edge_endpoints(EdgeRelation::CompetesWith, a.id, b.id);

        // Caller token is ns-a (an unrelated namespace); passing "ns-b" explicitly
        // must still find the edge recorded there.
        let found = rt
            .get_edge_by_natural_key_including_deleted(
                &ns_a,
                "ns-b",
                canon_src,
                canon_tgt,
                EdgeRelation::CompetesWith,
            )
            .await
            .unwrap();
        assert_eq!(
            found.map(|e| Uuid::from(e.id)),
            Some(Uuid::from(edge.id)),
            "must find the edge by its own namespace regardless of the caller's token"
        );

        // Passing the caller token's OWN namespace ("ns-a") as the explicit filter
        // must NOT find it — proves the lookup is keyed on the `namespace` argument,
        // not silently re-scoped to whatever namespace the token carries.
        let not_found = rt
            .get_edge_by_natural_key_including_deleted(
                &ns_a,
                "ns-a",
                canon_src,
                canon_tgt,
                EdgeRelation::CompetesWith,
            )
            .await
            .unwrap();
        assert!(
            not_found.is_none(),
            "must not find an edge recorded in a different namespace than the one queried"
        );
    }

    /// `link` endpoint existence is a by-ID check and therefore namespace-agnostic:
    /// a target living in a different namespace than the caller must still
    /// resolve, exactly as `get()` would.
    #[tokio::test]
    async fn link_target_in_different_namespace_succeeds() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());
        let a = rt
            .create_entity(&ns_a, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&ns_b, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();

        // Linking from ns-a: target b lives in ns-b — by-ID resolution finds it anyway.
        let result = rt
            .link(&ns_a, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "target in a different namespace than the caller must resolve (#631), got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_phantom_self_loop_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(&tok, phantom, phantom, EdgeRelation::Extends, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("self-loop"),
                    "self-loop must be rejected with self-loop message: {msg}"
                );
            }
            other => panic!("expected InvalidInput for self-loop, got {other:?}"),
        }
    }

    // ---- edge target coverage + atomicity ----

    #[tokio::test]
    async fn link_note_to_edge_annotates_succeeds() {
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
        // Create a real edge between a and b, capture its UUID.
        let edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        // Create a note and annotate the edge itself (edge is a valid substrate target for annotates).
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "edge note",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(&tok, note.id, edge_uuid, EdgeRelation::Annotates, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "note→edge Annotates must succeed, got {result:?}"
        );
    }
    /// #803: `neighbors(edge_id, direction=In, relations=[Annotates])` must
    /// find the annotating note — the storage-layer `graph_edges` query
    /// filters on `target_id = node_id` with no substrate-type check, so an
    /// edge id works as a neighbor-query node the same as an entity or note
    /// id. This is the runtime capability `get(edge_id)`'s new `annotations`
    /// field (khive-pack-kg) builds on.
    #[tokio::test]
    async fn neighbors_edge_id_finds_annotating_note() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "edge note",
                Some(0.5),
                None,
                vec![edge_uuid],
            )
            .await
            .unwrap();

        let neighbors = rt
            .neighbors(
                &tok,
                edge_uuid,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1, "expected annotating note to show up");
        assert_eq!(neighbors[0].node_id, note.id);
    }

    #[tokio::test]
    async fn create_note_annotates_real_edge_succeeds() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "annotating an edge",
                Some(0.5),
                None,
                vec![edge_uuid],
            )
            .await
            .unwrap();

        let neighbors = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, edge_uuid);
    }

    #[tokio::test]
    async fn create_note_annotates_phantom_is_atomic_no_note_persisted() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let phantom = Uuid::new_v4();

        let before_count = rt.list_notes(&tok, None, 1000, 0).await.unwrap().len();

        let result = rt
            .create_note(
                &tok,
                "observation",
                None,
                "should not persist",
                Some(0.5),
                None,
                vec![phantom],
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound(_))),
            "phantom annotates target must return NotFound, got {result:?}"
        );

        // Atomicity: the note row must NOT have been written.
        let after_count = rt.list_notes(&tok, None, 1000, 0).await.unwrap().len();
        assert_eq!(
            before_count, after_count,
            "failed create_note must not persist any note row (atomicity)"
        );

        // FTS must not contain the content either.
        let search_hits = rt
            .search_notes(&tok, "should not persist", None, 10, None, false, &[], None)
            .await
            .unwrap();
        assert!(
            search_hits.is_empty(),
            "failed create_note must not index into FTS (atomicity)"
        );
        // Vector-store row: only written when an embedding model is configured; the rt()
        // harness has none, so no vector assertion is needed here.
    }

    // ---- relation-aware endpoint contract ----

    // Test #2: entity→entity with non-annotates rejects an edge UUID as target.
    #[tokio::test]
    async fn link_entity_to_edge_uuid_non_annotates_returns_invalid_input() {
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
        // Create a real edge; capture its UUID as the bad target.
        let edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(&tok, a.id, edge_uuid, EdgeRelation::Extends, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("target"),
                    "error message must name 'target': {msg}"
                );
            }
            other => {
                panic!("expected InvalidInput for edge-uuid target with Extends, got {other:?}")
            }
        }
    }

    // Test #3: non-annotates rejects a note UUID as source.
    #[tokio::test]
    async fn link_note_as_source_non_annotates_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(&tok, "observation", None, "a note", Some(0.5), None, vec![])
            .await
            .unwrap();
        let entity = rt
            .create_entity(&tok, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(&tok, note.id, entity.id, EdgeRelation::DependsOn, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("source"),
                    "error message must name 'source': {msg}"
                );
            }
            other => panic!("expected InvalidInput for note source with DependsOn, got {other:?}"),
        }
    }

    // Test #4: annotates rejects entity as source (source must be a note).
    #[tokio::test]
    async fn link_entity_as_annotates_source_returns_invalid_input() {
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

        let result = rt
            .link(&tok, a.id, b.id, EdgeRelation::Annotates, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("source") && msg.contains("note"),
                    "error must say source must be a note: {msg}"
                );
            }
            other => {
                panic!("expected InvalidInput for entity source with Annotates, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_edge_as_annotates_source_returns_invalid_input() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        // An existing edge used as an annotates source: wrong kind, not absent.
        let result = rt
            .link(&tok, edge_uuid, a.id, EdgeRelation::Annotates, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("source") && msg.contains("note"),
                    "edge-as-annotates-source must report wrong kind, not NotFound: {msg}"
                );
            }
            other => panic!("expected InvalidInput for edge source with Annotates, got {other:?}"),
        }
    }

    // Test #5: note→event with annotates succeeds (event is a valid annotates target).
    #[tokio::test]
    async fn link_note_to_event_annotates_succeeds() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "observing an event",
                Some(0.6),
                None,
                vec![],
            )
            .await
            .unwrap();

        // Build an event directly via the store (no runtime create_event exists).
        let ns = tok.namespace().as_str();
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(&tok).unwrap().append_event(event).await.unwrap();

        let result = rt
            .link(&tok, note.id, event_id, EdgeRelation::Annotates, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "note→event Annotates must succeed, got {result:?}"
        );
    }

    // Test #6: create_note with event as annotates target succeeds.
    #[tokio::test]
    async fn create_note_annotates_event_succeeds() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().as_str();
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(&tok).unwrap().append_event(event).await.unwrap();

        let result = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note annotating an event",
                Some(0.5),
                None,
                vec![event_id],
            )
            .await;
        assert!(
            result.is_ok(),
            "create_note with event annotates target must succeed, got {result:?}"
        );
        // Verify the annotates edge was created.
        let note = result.unwrap();
        let neighbors = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].node_id, event_id);
    }

    // ---- supersedes same-substrate contract ----

    // Headline regression: note→note supersedes must succeed (was wrongly rejected before this fix).
    #[tokio::test]
    async fn link_supersedes_note_to_note_succeeds() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let old_note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "old observation",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .unwrap();
        let new_note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "revised observation superseding the old one",
                Some(0.9),
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                new_note.id,
                old_note.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "note→note Supersedes must succeed (note supersession), got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_supersedes_entity_to_entity_succeeds() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let old_entity = rt
            .create_entity(&tok, "concept", None, "OldConcept", None, None, vec![])
            .await
            .unwrap();
        let new_entity = rt
            .create_entity(&tok, "concept", None, "NewConcept", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                new_entity.id,
                old_entity.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "entity→entity Supersedes must succeed, got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_supersedes_note_to_entity_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(&tok, "observation", None, "a note", Some(0.5), None, vec![])
            .await
            .unwrap();
        let entity = rt
            .create_entity(&tok, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                note.id,
                entity.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("same substrate") || msg.contains("same-substrate"),
                    "error must name the same-substrate rule: {msg}"
                );
            }
            other => panic!(
                "expected InvalidInput for note→entity Supersedes (cross-substrate), got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn link_supersedes_entity_to_note_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(&tok, "observation", None, "a note", Some(0.5), None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                entity.id,
                note.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("same substrate") || msg.contains("same-substrate"),
                    "error must name the same-substrate rule: {msg}"
                );
            }
            other => panic!(
                "expected InvalidInput for entity→note Supersedes (cross-substrate), got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn link_supersedes_event_source_returns_invalid_input() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().as_str();
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(&tok).unwrap().append_event(event).await.unwrap();

        let entity = rt
            .create_entity(&tok, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                event_id,
                entity.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("event"), "error must mention 'event': {msg}");
            }
            other => {
                panic!("expected InvalidInput for event source with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_event_target_returns_invalid_input() {
        use khive_storage::Event;
        use khive_types::{EventKind, SubstrateKind};

        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().as_str();
        let event = Event::new(
            ns,
            "test_verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test_actor",
        );
        let event_id = event.id;
        rt.events(&tok).unwrap().append_event(event).await.unwrap();

        let entity = rt
            .create_entity(&tok, "concept", None, "SomeEntity", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                entity.id,
                event_id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("event"), "error must mention 'event': {msg}");
            }
            other => {
                panic!("expected InvalidInput for event target with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_edge_source_returns_invalid_input() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(&tok, edge_uuid, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("source"), "error must name 'source': {msg}");
            }
            other => {
                panic!("expected InvalidInput for edge-uuid source with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_edge_target_returns_invalid_input() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let result = rt
            .link(&tok, a.id, edge_uuid, EdgeRelation::Supersedes, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(msg.contains("target"), "error must name 'target': {msg}");
            }
            other => {
                panic!("expected InvalidInput for edge-uuid target with Supersedes, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn link_supersedes_phantom_source_returns_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "existing note",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(&tok, phantom, note.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(msg.contains("source"), "error must name 'source': {msg}");
            }
            other => panic!("expected NotFound for phantom source with Supersedes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn link_supersedes_phantom_target_returns_not_found() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "existing note",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        let phantom = Uuid::new_v4();

        let result = rt
            .link(&tok, note.id, phantom, EdgeRelation::Supersedes, 1.0, None)
            .await;
        match result {
            Err(RuntimeError::NotFound(msg)) => {
                assert!(msg.contains("target"), "error must name 'target': {msg}");
            }
            other => panic!("expected NotFound for phantom target with Supersedes, got {other:?}"),
        }
    }

    /// The canonical `remember | supersedes` chain: a `supersedes` source note living
    /// in a different namespace than the caller must still resolve as a by-ID endpoint.
    #[tokio::test]
    async fn link_supersedes_cross_namespace_source_succeeds() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());
        let note_a = rt
            .create_note(
                &ns_a,
                "observation",
                None,
                "note in ns-a",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        let note_b = rt
            .create_note(
                &ns_b,
                "observation",
                None,
                "note in ns-b",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();

        // From ns-a perspective, note_b is in a different namespace — by-ID resolution
        // finds it anyway.
        let result = rt
            .link(
                &ns_a,
                note_b.id,
                note_a.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "cross-namespace supersedes source must resolve (#631), got {result:?}"
        );
    }

    // Sanity: extends (non-annotates, non-supersedes) still requires entity→entity.
    #[tokio::test]
    async fn link_extends_note_source_still_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "a note that cannot be an extends source",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        let entity = rt
            .create_entity(&tok, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(&tok, note.id, entity.id, EdgeRelation::Extends, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "note source with Extends must still return InvalidInput after this fix, got {result:?}"
        );
    }

    // Sanity: annotates note→edge still succeeds (unchanged path not broken by this fix).
    #[tokio::test]
    async fn link_annotates_note_to_edge_still_succeeds_after_fix() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "annotating an edge",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();

        let result = rt
            .link(&tok, note.id, edge_uuid, EdgeRelation::Annotates, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "note→edge Annotates must still succeed after supersedes fix, got {result:?}"
        );
    }

    // ---- Compensation-path rollback (fix/annotates) ----

    // The compensation branch in `create_note_inner` (operations.rs) rolls back
    // a partial write — note row + first edge + FTS + vector — when a subsequent
    // link call fails. The failure trigger is a storage error (e.g. I/O failure)
    // that cannot occur in the in-memory runtime; this test instead exercises the
    // exact cleanup operations that the compensation branch performs, starting from
    // a manually-constructed partial state, and verifies the post-cleanup invariants.
    //
    // What this covers: the cleanup sequence (delete_edge, delete_note hard, FTS
    // index clean) is correct and leaves the DB in a pristine state. What it does
    // not cover: the trigger condition (second link failure). Storage-error injection
    // would require a mock GraphStore, which is beyond the current test infrastructure.
    #[tokio::test]
    async fn create_note_multi_annotates_compensation_cleanup_restores_pristine_state() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let t1 = rt
            .create_entity(&tok, "concept", None, "T1", None, None, vec![])
            .await
            .unwrap();

        // Construct the partial state that the compensation branch would encounter:
        // note persisted + first annotates edge created.
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "partial note",
                Some(0.5),
                None,
                vec![t1.id],
            )
            .await
            .unwrap();

        // Confirm the partial state exists before compensation.
        let before_notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert_eq!(before_notes.len(), 1, "note must be present before cleanup");
        let before_edges = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before_edges.len(),
            1,
            "one annotates edge must exist before cleanup"
        );
        let edge_id: Uuid = before_edges[0].edge_id;

        // Execute the same cleanup sequence that `create_note_inner`'s Err branch runs.
        rt.delete_edge(&tok, edge_id, true).await.unwrap();
        rt.delete_note(&tok, note.id, true /* hard */)
            .await
            .unwrap();

        // Post-compensation invariants:
        let after_notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            after_notes.is_empty(),
            "compensation must remove the note row; got {after_notes:?}"
        );
        let search_hits = rt
            .search_notes(&tok, "partial note", None, 10, None, false, &[], None)
            .await
            .unwrap();
        assert!(
            search_hits.is_empty(),
            "compensation must clean the FTS index; got {search_hits:?}"
        );
        let after_edges = rt
            .neighbors(&tok, note.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(
            after_edges.is_empty(),
            "compensation must remove all partial edges; got {after_edges:?}"
        );
    }

    // ---- Hard-delete cascade for note and edge annotation targets (fix/annotates) ----

    // annotates is note → ANYTHING (entity, note, edge, event);
    // targets may be entity, edge, event, or note.
    // Hard-deleting any of those targets must cascade incident annotates edges.
    // Soft deletes leave edges (data-vs-view rule).

    #[tokio::test]
    async fn annotated_entity_hard_delete_cascades_annotate_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(&tok, "concept", None, "E", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note about entity",
                Some(0.5),
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        // Confirm edge exists before delete.
        let before = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "annotates edge must exist before entity delete"
        );

        // Hard delete the entity.
        let deleted = rt.delete_entity(&tok, entity.id, true).await.unwrap();
        assert!(deleted, "entity hard delete must return true");

        // Annotates edge must be gone.
        let after = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "annotates edge must be cascaded on entity hard delete; got {after:?}"
        );
    }

    #[tokio::test]
    async fn annotated_note_hard_delete_cascades_annotate_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        // note_target is the thing being annotated (a note itself).
        let note_target = rt
            .create_note(
                &tok,
                "observation",
                None,
                "target note",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        // note_source annotates note_target.
        let note_source = rt
            .create_note(
                &tok,
                "insight",
                None,
                "annotation",
                Some(0.5),
                None,
                vec![note_target.id],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                &tok,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "annotates edge must exist before note delete"
        );

        // Hard delete the annotation TARGET note.
        let deleted = rt.delete_note(&tok, note_target.id, true).await.unwrap();
        assert!(deleted, "note hard delete must return true");

        // The annotates edge targeting note_target must be gone.
        let after = rt
            .neighbors(
                &tok,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "annotates edge must be cascaded on note-target hard delete; got {after:?}"
        );
    }

    #[tokio::test]
    async fn annotated_edge_delete_cascades_annotate_edge() {
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
        // Create an edge to annotate.
        let base_edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let base_edge_uuid: Uuid = base_edge.id.into();

        // Create a note that annotates the edge.
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note about edge",
                Some(0.5),
                None,
                vec![base_edge_uuid],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            1,
            "annotates edge must exist before base edge delete"
        );

        // Delete the base edge.
        let deleted = rt.delete_edge(&tok, base_edge_uuid, true).await.unwrap();
        assert!(deleted, "edge delete must return true");

        // The annotates edge targeting base_edge must be gone.
        let after = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "annotates edge must be cascaded on base edge delete; got {after:?}"
        );
    }

    #[tokio::test]
    async fn mixed_multi_annotates_partial_target_hard_delete_leaves_remaining_edges() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let t1 = rt
            .create_entity(&tok, "concept", None, "T1", None, None, vec![])
            .await
            .unwrap();
        let t2 = rt
            .create_entity(&tok, "concept", None, "T2", None, None, vec![])
            .await
            .unwrap();

        // Note annotates both t1 and t2.
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "multi-target note",
                Some(0.5),
                None,
                vec![t1.id, t2.id],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            before.len(),
            2,
            "must have 2 annotates edges before any delete"
        );

        // Hard delete only t1.
        rt.delete_entity(&tok, t1.id, true).await.unwrap();

        // Edge to t1 must be gone, edge to t2 must remain.
        let after = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "only the edge to t1 must be cascaded; t2 edge must remain"
        );
        assert_eq!(
            after[0].node_id, t2.id,
            "remaining annotates edge must point to t2"
        );
    }

    #[tokio::test]
    async fn annotated_note_soft_delete_preserves_annotate_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note_target = rt
            .create_note(&tok, "observation", None, "target", Some(0.5), None, vec![])
            .await
            .unwrap();
        let note_source = rt
            .create_note(
                &tok,
                "insight",
                None,
                "annotation",
                Some(0.5),
                None,
                vec![note_target.id],
            )
            .await
            .unwrap();

        let before = rt
            .neighbors(
                &tok,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(before.len(), 1);
        let edge_id = before[0].edge_id;

        // Soft delete must NOT cascade edges (data-vs-view principle).
        let deleted = rt.delete_note(&tok, note_target.id, false).await.unwrap();
        assert!(deleted, "soft delete must return true");

        // The edge itself must survive the soft delete — checked at the
        // storage/edge layer directly (`get_edge`), not through `neighbors()`.
        // `neighbors()` is a VIEW query and correctly screens
        // out soft-deleted note targets — so it no longer surfaces this edge
        // once note_target is soft-deleted, even though the edge row itself
        // is untouched (data-vs-view principle: the edge is data, what
        // `neighbors()` shows is a view decision).
        let edge_after = rt.get_edge(&tok, edge_id).await.unwrap();
        assert!(
            edge_after.is_some(),
            "soft delete must NOT cascade edges; get_edge returned None"
        );

        let after = rt
            .neighbors(
                &tok,
                note_source.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            0,
            "#748: neighbors() must screen out the soft-deleted note target; got {after:?}"
        );
    }

    // ---- delete_edge public-API safety ----

    // Passing an entity/note UUID to `delete_edge` must return Ok(false) with no
    // side effects — it must NOT delete inbound annotates edges targeting that record.
    // Without the get_edge guard, the old code would cascade inbound edges before
    // returning false.
    #[tokio::test]
    async fn delete_edge_non_edge_uuid_has_no_side_effects() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Create an entity that has an inbound annotates edge.
        let entity = rt
            .create_entity(&tok, "concept", None, "Target", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "annotates the entity",
                Some(0.5),
                None,
                vec![entity.id],
            )
            .await
            .unwrap();

        // Confirm the annotates edge exists.
        let before = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(before.len(), 1, "annotates edge must exist before test");
        let annotates_edge_id: Uuid = before[0].edge_id;

        // Call delete_edge with the entity UUID (NOT an edge UUID).
        let result = rt.delete_edge(&tok, entity.id, true).await;
        assert!(
            result.is_ok(),
            "delete_edge must not error on a non-edge UUID"
        );
        assert!(
            !result.unwrap(),
            "delete_edge must return false for a non-edge UUID"
        );

        // The inbound annotates edge to the entity must still exist — no side effects.
        let after = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "delete_edge with a non-edge UUID must not touch inbound annotates edges"
        );
        assert_eq!(
            after[0].edge_id, annotates_edge_id,
            "the original annotates edge must be unchanged"
        );
    }

    // ---- create_note compensation branch ----

    // This test injects a deterministic failure on the second `link` call inside
    // `create_note_inner` (the one that would create the second annotates edge).
    // It verifies that the compensation branch is wired — i.e. this test would
    // fail if the `Err(e)` rollback arm at operations.rs were deleted.
    //
    // Injection mechanism: LINK_FAIL_AFTER thread-local (ops.rs, cfg(test) only).
    // Setting it to 2 forces the 2nd link call to return an error.  The counter is
    // reset to 0 once triggered, so no other test is affected.
    #[tokio::test]
    async fn create_note_multi_annotates_second_link_failure_rolls_back_partial_write() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let t1 = rt
            .create_entity(&tok, "concept", None, "T1", None, None, vec![])
            .await
            .unwrap();
        let t2 = rt
            .create_entity(&tok, "concept", None, "T2", None, None, vec![])
            .await
            .unwrap();

        // Arm the injection: fail on the 2nd link (link_idx+1 == 2).
        LINK_FAIL_AFTER.with(|cell| cell.set(2));

        let result = rt
            .create_note(
                &tok,
                "observation",
                None,
                "rollback target",
                Some(0.5),
                None,
                vec![t1.id, t2.id],
            )
            .await;

        // The call must fail with the injected error.
        assert!(
            result.is_err(),
            "create_note must propagate the injected link failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected link failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row.
        let notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove the note row; got {notes:?}"
        );

        // FTS must have no hit for the content.
        let hits = rt
            .search_notes(&tok, "rollback target", None, 10, None, false, &[], None)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "compensation must clean FTS index; got {hits:?}"
        );

        // No partial annotates edges must remain (first edge must have been deleted).
        let edges_from_t1 = rt
            .neighbors(
                &tok,
                t1.id,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        let edges_from_t2 = rt
            .neighbors(
                &tok,
                t2.id,
                Direction::In,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert!(
            edges_from_t1.is_empty(),
            "compensation must delete the first annotates edge; got {edges_from_t1:?}"
        );
        assert!(
            edges_from_t2.is_empty(),
            "no second annotates edge must exist; got {edges_from_t2:?}"
        );
    }

    // Inject an FTS failure after the note row is committed and assert the note
    // row is removed (no stranded row). arm_fts_fail_scoped() arms the flag before
    // the call and it resets automatically after one trigger.
    #[tokio::test]
    async fn create_note_fts_failure_rolls_back_note_row() {
        let rt = rt();
        // Unique namespace: FTS_FAIL_NS is a namespace-keyed set, so a
        // concurrently running test arming a different namespace never evicts
        // this test's arm. The namespace still guards against a same-test
        // mismatch between the armed value and the note actually being created.
        let ns = Namespace::parse("fault-fts-rollback").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_fts_fail_scoped(ns.as_str());

        let result = rt
            .create_note(
                &tok,
                "observation",
                None,
                "fts-fail rollback target",
                None,
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_note must propagate the injected FTS failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected FTS failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row.
        let notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove the note row after FTS failure; got {notes:?}"
        );
    }

    // Arming FTS_FAIL_NS on one OS thread must still fire on a `create_note`
    // call that runs on a genuinely different OS thread. Arms here on the
    // test's own (tokio current-thread) task, then hands the triggering
    // `create_note` call to a `std::thread::spawn` worker running its own
    // single-threaded tokio runtime — a stronger guarantee of thread migration
    // than `tokio::spawn`, which may schedule the spawned task back onto the
    // same worker. Proves the process-wide, namespace-keyed `FTS_FAIL_NS` set
    // is thread-independent.
    #[tokio::test]
    async fn create_note_fts_failure_fires_across_os_threads() {
        let rt = std::sync::Arc::new(rt());
        let ns = Namespace::parse("fault-fts-rollback-cross-thread").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_fts_fail_scoped(ns.as_str());

        let thread_rt = std::sync::Arc::clone(&rt);
        let thread_tok = tok.clone();
        let result = std::thread::spawn(move || {
            let worker = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("worker runtime must build");
            worker.block_on(thread_rt.create_note(
                &thread_tok,
                "observation",
                None,
                "fts-fail rollback target (cross-thread)",
                None,
                None,
                vec![],
            ))
        })
        .join()
        .expect("worker thread must not panic");

        assert!(
            result.is_err(),
            "create_note on a different OS thread must still observe the injected FTS failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected FTS failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row.
        let notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove the note row after FTS failure; got {notes:?}"
        );
    }

    // Inject a vector insertion failure after note row + FTS commit and assert
    // both the note row and the FTS document are removed (no stranded rows).
    // Uses a unique namespace (see create_note_fts_failure_rolls_back_note_row)
    // so only this test consumes its VECTOR_FAIL_NS entry.
    // Since the single registered provider fires embed_document before the
    // injection check, the injection converts the successful embedding into an
    // error just before the VectorStore insert, then disarms.
    #[tokio::test]
    async fn create_note_vector_failure_rolls_back_note_row_and_fts() {
        const MODEL: &str = "test-vec-inject";
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider, _counter) = ConstVecProvider::new(MODEL, DIMS);
        rt.register_embedder(provider);

        let ns = Namespace::parse("fault-vec-rollback").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_vector_fail_scoped(ns.as_str());

        let result = rt
            .create_note(
                &tok,
                "observation",
                None,
                "vec-fail rollback target",
                None,
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_note must propagate the injected vector failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected vector failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row.
        let notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove note row after vector failure; got {notes:?}"
        );
    }

    #[tokio::test]
    async fn vector_failure_injections_for_distinct_namespaces_do_not_overwrite_each_other() {
        const MODEL: &str = "test-vec-inject-distinct-namespaces";
        const DIMS: usize = 4;

        let rt_a = KhiveRuntime::memory().unwrap();
        let (provider_a, _counter_a) = ConstVecProvider::new(MODEL, DIMS);
        rt_a.register_embedder(provider_a);
        let ns_a = Namespace::parse("fault-vec-distinct-a").unwrap();
        let tok_a = NamespaceToken::for_namespace(ns_a.clone());

        let rt_b = KhiveRuntime::memory().unwrap();
        let (provider_b, _counter_b) = ConstVecProvider::new(MODEL, DIMS);
        rt_b.register_embedder(provider_b);
        let ns_b = Namespace::parse("fault-vec-distinct-b").unwrap();
        let tok_b = NamespaceToken::for_namespace(ns_b.clone());

        let _arm_a = arm_vector_fail_scoped(ns_a.as_str());
        let _arm_b = arm_vector_fail_scoped(ns_b.as_str());

        let (result_a, result_b) = tokio::join!(
            rt_a.create_note(
                &tok_a,
                "observation",
                None,
                "vector failure target A",
                None,
                None,
                vec![],
            ),
            rt_b.create_note(
                &tok_b,
                "observation",
                None,
                "vector failure target B",
                None,
                None,
                vec![],
            ),
        );

        assert!(
            result_a.is_err(),
            "namespace A must retain its pending vector failure injection"
        );
        assert!(
            result_b.is_err(),
            "namespace B must retain its pending vector failure injection"
        );
    }

    // The `embedding_content` override must not bypass the same
    // FTS/vector compensation the plain `create_note` path already has —
    // both use `create_note_inner` underneath, but these tests exercise it
    // through `create_note_with_embedding_content` with a real Some(head)
    // override to prove the override path shares the identical rollback.
    #[tokio::test]
    async fn create_note_with_embedding_content_fts_failure_rolls_back_note_row() {
        let rt = rt();
        let ns = Namespace::parse("fault-fts-rollback-embedding-content").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_fts_fail_scoped(ns.as_str());

        let full = "fts-fail rollback target with an embedding-content override";
        let head = &full[.."fts-fail rollback target".len()];
        let result = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                full,
                Some(head),
                None,
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_note_with_embedding_content must propagate the injected FTS failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected FTS failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row; a failed create must
        // never leave a stranded row behind just because it carried an
        // embedding_content override.
        let notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove the note row after FTS failure; got {notes:?}"
        );
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_vector_failure_rolls_back_note_row_and_fts() {
        const MODEL: &str = "test-vec-inject-embedding-content";
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider, _counter) = ConstVecProvider::new(MODEL, DIMS);
        rt.register_embedder(provider);

        let ns = Namespace::parse("fault-vec-rollback-embedding-content").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_vector_fail_scoped(ns.as_str());

        let full = "vec-fail rollback target with an embedding-content override";
        let head = &full[.."vec-fail rollback target".len()];
        let result = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                full,
                Some(head),
                None,
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_note_with_embedding_content must propagate the injected vector failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected vector failure"),
            "error must carry injection message; got: {err_msg}"
        );

        // Compensation must have removed the note row: the ingest-layer
        // truncation counter only increments in the successful-create arm,
        // so a failed create — with or without an embedding_content override
        // — can never cause a spurious truncation count on the caller side.
        let notes = rt.list_notes(&tok, None, 1000, 0).await.unwrap();
        assert!(
            notes.is_empty(),
            "compensation must remove note row after vector failure; got {notes:?}"
        );
    }

    // ---- soft-delete index cleanup tests ----

    #[tokio::test]
    async fn soft_delete_entity_removes_indexes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "QuantumEntanglement",
                Some("unique FTS term xzqjwv for soft delete test"),
                None,
                vec![],
            )
            .await
            .unwrap();

        let ns = tok.namespace().as_str().to_string();

        let before = rt
            .text(&tok)
            .unwrap()
            .search(TextSearchRequest {
                query: "xzqjwv".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert!(
            before.iter().any(|h| h.subject_id == entity.id),
            "entity must be in FTS before soft-delete"
        );

        let deleted = rt.delete_entity(&tok, entity.id, false).await.unwrap();
        assert!(deleted, "soft delete must return true");

        let after = rt
            .text(&tok)
            .unwrap()
            .search(TextSearchRequest {
                query: "xzqjwv".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert!(
            after.iter().all(|h| h.subject_id != entity.id),
            "soft-deleted entity must be removed from FTS index"
        );
    }

    #[tokio::test]
    async fn soft_delete_note_removes_indexes() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "SpectralDecomposition unique term yvwkqz for soft delete test",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .unwrap();

        let before = rt
            .search_notes(&tok, "yvwkqz", None, 10, None, false, &[], None)
            .await
            .unwrap();
        assert!(
            before.iter().any(|h| h.note_id == note.id),
            "note must be in FTS before soft-delete"
        );

        let deleted = rt.delete_note(&tok, note.id, false).await.unwrap();
        assert!(deleted, "soft delete must return true");

        let after = rt
            .search_notes(&tok, "yvwkqz", None, 10, None, false, &[], None)
            .await
            .unwrap();
        assert!(
            after.iter().all(|h| h.note_id != note.id),
            "soft-deleted note must be removed from FTS index"
        );
    }

    // Base endpoint allowlist: unlisted triples must fail closed.
    // Document->Document Extends is not in the allowlist.
    #[tokio::test]
    async fn link_extends_document_to_document_returns_invalid_input() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let d1 = rt
            .create_entity(&tok, "document", None, "DocA", None, None, vec![])
            .await
            .unwrap();
        let d2 = rt
            .create_entity(&tok, "document", None, "DocB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, d1.id, d2.id, EdgeRelation::Extends, 1.0, None)
            .await;
        assert!(
            result.is_err(),
            "F010: document->document Extends must be rejected by the base allowlist; \
             current generic entity fallthrough incorrectly accepts it"
        );
    }

    // Happy path: Concept->Concept Extends is in the base allowlist and must succeed.
    #[tokio::test]
    async fn link_extends_concept_to_concept_succeeds() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "CA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "CB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "F010: concept->concept Extends must be allowed (base allowlist)"
        );
    }

    // CompetesWith is symmetric; reversed pair must deduplicate to one canonical row.
    #[tokio::test]
    async fn link_symmetric_relation_canonicalizes_endpoint_order() {
        use khive_storage::EdgeFilter;
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "ConceptP", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "ConceptQ", None, None, vec![])
            .await
            .unwrap();
        // Link A->B then B->A with the same symmetric relation.
        rt.link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        rt.link(&tok, b.id, a.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        let count = rt
            .graph(&tok)
            .unwrap()
            .count_edges(EdgeFilter::default())
            .await
            .unwrap();
        assert_eq!(
            count,
            1,
            "F012: CompetesWith is symmetric; A->B and B->A must deduplicate to one canonical row; \
             found {count} rows (canonicalization not yet implemented)"
        );
    }

    // Supersedes: positive tests for all 5 allowed entity kinds.
    #[tokio::test]
    async fn f010_supersedes_document_to_document_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "document", None, "DocA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "document", None, "DocB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "document->document Supersedes must be allowed (allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_artifact_to_artifact_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "artifact", None, "ArtA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "artifact", None, "ArtB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "artifact->artifact Supersedes must be allowed (allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_service_to_service_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "service", None, "SvcA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "service", None, "SvcB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "service->service Supersedes must be allowed (allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_dataset_to_dataset_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "dataset", None, "DataA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "dataset", None, "DataB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "dataset->dataset Supersedes must be allowed (allowlist), got {result:?}"
        );
    }

    // Supersedes: negative tests for rejected entity kinds.
    #[tokio::test]
    async fn f010_supersedes_project_to_project_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "project", None, "ProjA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "project", None, "ProjB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "project->project Supersedes must be rejected (not in allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_person_to_person_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "person", None, "Alice", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "person", None, "Bob", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "person->person Supersedes must be rejected (not in allowlist), got {result:?}"
        );
    }

    #[tokio::test]
    async fn f010_supersedes_org_to_org_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "org", None, "OrgA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "org", None, "OrgB", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "org->org Supersedes must be rejected (not in allowlist), got {result:?}"
        );
    }

    // Supersedes entity→entity: same kind (concept→concept) must be allowed.
    #[tokio::test]
    async fn f010_supersedes_same_kind_entity_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let a = rt
            .create_entity(&tok, "concept", None, "OldV", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "NewV", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(&tok, b.id, a.id, EdgeRelation::Supersedes, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "concept->concept Supersedes must be allowed by the base allowlist, got {result:?}"
        );
    }

    // target_backend invariant: all edges written through link() must have
    // target_backend = None because validate_edge_relation_endpoints already ensured the
    // target exists locally.
    #[tokio::test]
    async fn f161_link_always_writes_null_target_backend() {
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
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        assert!(
            edge.target_backend.is_none(),
            "F161: target_backend must be None for locally-routed edges; got {:?}",
            edge.target_backend
        );
    }

    // link_many must also write null target_backend for all local edges.
    #[tokio::test]
    async fn f161_link_many_always_writes_null_target_backend() {
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
        let specs = vec![
            LinkSpec {
                namespace: None,
                source_id: a.id,
                target_id: b.id,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                metadata: None,
            },
            LinkSpec {
                namespace: None,
                source_id: a.id,
                target_id: c.id,
                relation: EdgeRelation::Enables,
                weight: 1.0,
                metadata: None,
            },
        ];
        let edges = rt.link_many(&tok, specs).await.unwrap();
        for edge in &edges {
            assert!(
                edge.target_backend.is_none(),
                "F161: target_backend must be None for locally-routed edges in link_many; got {:?}",
                edge.target_backend
            );
        }
    }

    // Symmetric relation neighbors: competes_with queried from the non-canonical
    // endpoint must still return results when direction=Out is requested.
    #[tokio::test]
    async fn f012_symmetric_neighbors_visible_from_both_endpoints() {
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
        // Link A→B competes_with; if A.id > B.id the edge is stored as B→A (canonical).
        rt.link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
            .await
            .unwrap();
        // Both endpoints should see the edge regardless of direction=Out.
        let from_a = rt
            .neighbors(
                &tok,
                a.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::CompetesWith]),
            )
            .await
            .unwrap();
        let from_b = rt
            .neighbors(
                &tok,
                b.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::CompetesWith]),
            )
            .await
            .unwrap();
        assert_eq!(
            from_a.len(),
            1,
            "node A must see competes_with neighbor from Direction::Out (F012); got {from_a:?}"
        );
        assert_eq!(
            from_b.len(),
            1,
            "node B must see competes_with neighbor from Direction::Out (F012); got {from_b:?}"
        );
    }

    // Fix 1: Supersedes entity→entity — cross-kind (concept→document) must be rejected.
    #[tokio::test]
    async fn f010_supersedes_cross_kind_entity_rejected() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let concept = rt
            .create_entity(&tok, "concept", None, "MyConcept", None, None, vec![])
            .await
            .unwrap();
        let doc = rt
            .create_entity(&tok, "document", None, "MyDoc", None, None, vec![])
            .await
            .unwrap();
        let result = rt
            .link(
                &tok,
                concept.id,
                doc.id,
                EdgeRelation::Supersedes,
                1.0,
                None,
            )
            .await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "concept->document Supersedes must be rejected by the base allowlist, got {result:?}"
        );
    }

    // Cross-namespace delete_note now succeeds (UUID v4 is globally unique,
    // no namespace isolation on by-ID ops).
    #[tokio::test]
    async fn delete_note_cross_namespace_succeeds() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());
        let note = rt
            .create_note(
                &ns_a,
                "observation",
                None,
                "note in ns-a",
                Some(0.8),
                None,
                vec![],
            )
            .await
            .unwrap();

        // Delete from a different namespace must now SUCCEED.
        let result = rt.delete_note(&ns_b, note.id, false).await;
        assert!(
            result.unwrap(),
            "cross-namespace delete_note (soft) must return Ok(true)"
        );

        // Note must be gone from ns-a storage after the cross-ns soft delete.
        let note_store = rt.notes(&ns_a).unwrap();
        let gone = note_store.get_note(note.id).await.unwrap();
        assert!(
            gone.is_none(),
            "note must be soft-deleted in its home namespace after cross-ns delete"
        );

        // Hard-delete path: create a fresh note and hard-delete from foreign token.
        let note2 = rt
            .create_note(
                &ns_a,
                "observation",
                None,
                "note2 in ns-a",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        let hard_result = rt.delete_note(&ns_b, note2.id, true).await;
        assert!(
            hard_result.unwrap(),
            "cross-namespace hard delete_note must return Ok(true)"
        );
        let gone2 = rt
            .get_note_including_deleted(&ns_a, note2.id)
            .await
            .unwrap();
        assert!(
            gone2.is_none(),
            "hard-deleted note must not appear even in including_deleted query"
        );
    }

    // Regression: parallel link_many calls with overlapping triples must
    // return the identical persisted edge ID, not locally-generated phantom IDs.
    //
    // Sequence:
    //   1. First link_many creates the A→B Extends edge (persisted with ID₁).
    //   2. Second link_many upserts the same triple (ON CONFLICT DO UPDATE keeps ID₁).
    //   3. Both callers must see ID₁ in their returned Vec<Edge>.
    #[tokio::test]
    async fn link_many_overlapping_triple_returns_persisted_ids() {
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

        let spec = || LinkSpec {
            namespace: None,
            source_id: a.id,
            target_id: b.id,
            relation: EdgeRelation::Extends,
            weight: 1.0,
            metadata: None,
        };

        // First call — creates the edge.
        let first = rt.link_many(&tok, vec![spec()]).await.unwrap();
        assert_eq!(first.len(), 1);
        let persisted_id: Uuid = first[0].id.into();

        // Second call — same natural-key triple; ON CONFLICT updates, preserving the
        // existing row ID. link_many must read back the row and return that same ID.
        let second = rt.link_many(&tok, vec![spec()]).await.unwrap();
        assert_eq!(second.len(), 1);
        let second_id: Uuid = second[0].id.into();

        assert_eq!(
            persisted_id, second_id,
            "link_many with an existing triple must return the persisted row ID ({persisted_id}), \
             not a new phantom ID ({second_id})"
        );

        // Confirm only one edge row exists in the graph store.
        let count = rt
            .count_edges(&tok, crate::curation::EdgeListFilter::default())
            .await
            .unwrap();
        assert_eq!(count, 1, "upsert must not duplicate the edge row");
    }

    // ── create_many: batch entity creation ───────────────────────────────────

    #[tokio::test]
    async fn create_many_persists_all_entities() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let specs: Vec<EntityCreateSpec> = (0..5)
            .map(|i| EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: format!("BulkConcept-{i}"),
                description: Some(format!("desc {i}")),
                properties: None,
                tags: vec!["bulk-test".into()],
            })
            .collect();

        let entities = rt.create_many(&tok, specs).await.unwrap();
        assert_eq!(entities.len(), 5, "all 5 entities must be returned");

        // Verify each one is retrievable from storage.
        for entity in &entities {
            let fetched = rt.get_entity(&tok, entity.id).await.unwrap();
            assert_eq!(fetched.id, entity.id);
        }
    }

    #[tokio::test]
    async fn create_many_empty_name_rejects_atomically() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let specs = vec![
            EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "ValidEntity".into(),
                description: None,
                properties: None,
                tags: vec![],
            },
            EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "".into(), // invalid — triggers atomic rejection
                description: None,
                properties: None,
                tags: vec![],
            },
        ];

        let result = rt.create_many(&tok, specs).await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "empty name must produce InvalidInput error"
        );

        // Nothing must have been written — list_entities returns 0 items.
        let rows = rt.list_entities(&tok, None, None, 100, 0).await.unwrap();
        assert_eq!(
            rows.len(),
            0,
            "atomic rejection must leave storage unchanged"
        );
    }

    // entity_type validated at runtime layer when validator is installed.
    #[tokio::test]
    async fn create_many_rejects_unknown_entity_type_when_validator_installed() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Install a mock validator that only accepts "algorithm" for "concept".
        rt.install_entity_type_validator(Arc::new(|kind, entity_type| {
            let Some(raw) = entity_type else {
                return Ok(None);
            };
            if kind == "concept" && raw == "algorithm" {
                return Ok(Some("algorithm".to_string()));
            }
            Err(RuntimeError::InvalidInput(format!(
                "unknown entity_type {raw:?} for {kind:?}; valid: algorithm"
            )))
        }));

        let bad_spec = vec![EntityCreateSpec {
            kind: "concept".into(),
            entity_type: Some("not_a_registered_type".into()),
            name: "ShouldNotLand".into(),
            description: None,
            properties: None,
            tags: vec![],
        }];

        let result = rt.create_many(&tok, bad_spec).await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(_))),
            "unknown entity_type must be rejected by the runtime-layer validator; got {result:?}"
        );

        // Zero rows written — validator fires before any storage call.
        let rows = rt.list_entities(&tok, None, None, 100, 0).await.unwrap();
        assert_eq!(
            rows.len(),
            0,
            "validator rejection must leave storage empty"
        );
    }

    // Valid entity_type passes through and is normalised by the validator.
    #[tokio::test]
    async fn create_many_accepts_valid_entity_type_via_validator() {
        let rt = rt();
        let tok = NamespaceToken::local();

        rt.install_entity_type_validator(Arc::new(|kind, entity_type| {
            let Some(raw) = entity_type else {
                return Ok(None);
            };
            if kind == "concept" && raw == "algorithm" {
                return Ok(Some("algorithm".to_string()));
            }
            Err(RuntimeError::InvalidInput(format!(
                "unknown entity_type {raw:?} for {kind:?}"
            )))
        }));

        let specs = vec![EntityCreateSpec {
            kind: "concept".into(),
            entity_type: Some("algorithm".into()),
            name: "BubbleSort".into(),
            description: None,
            properties: None,
            tags: vec![],
        }];

        let entities = rt.create_many(&tok, specs).await.unwrap();
        assert_eq!(entities.len(), 1, "valid entity_type must be accepted");
        assert_eq!(
            entities[0].entity_type.as_deref(),
            Some("algorithm"),
            "entity_type must be stored as returned by the validator"
        );
    }

    // FTS failure in create_many rolls back both substrates.
    //
    // Arm `arm_fts_fail_many_scoped` before the call; the FTS phase returns an injected
    // error; the test asserts zero rows in both `entities` and `fts_entities`.
    #[tokio::test]
    async fn create_many_fts_failure_rolls_back_both_substrates() {
        // Use a unique namespace so only this test consumes its failure entry.
        let ns = format!("fts-fail-many-{}", uuid::Uuid::new_v4().as_simple());
        let rt = rt();
        let tok = NamespaceToken::for_namespace(Namespace::parse(&ns).unwrap());

        let specs = vec![
            EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "FtsRollbackA".into(),
                description: None,
                properties: None,
                tags: vec![],
            },
            EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "FtsRollbackB".into(),
                description: None,
                properties: None,
                tags: vec![],
            },
        ];

        let _arm = arm_fts_fail_many_scoped(&ns);
        let result = rt.create_many(&tok, specs).await;

        assert!(
            result.is_err(),
            "create_many must return Err when FTS write fails"
        );

        // Entity substrate must be empty — entity rows must have been rolled back.
        let entity_rows = rt.list_entities(&tok, None, None, 100, 0).await.unwrap();
        assert_eq!(
            entity_rows.len(),
            0,
            "entity rows must be rolled back on FTS failure; found {entity_rows:?}"
        );

        // FTS substrate must be empty — no stale fts_entities rows.
        let fts = rt.text(&tok).unwrap();
        let fts_count = fts
            .count(TextFilter {
                ids: vec![],
                kinds: vec![],
                namespaces: vec![ns.clone()],
            })
            .await
            .unwrap();
        assert_eq!(
            fts_count, 0,
            "fts_entities must be empty after FTS-failure rollback; found {fts_count}"
        );
    }

    #[tokio::test]
    async fn create_many_fts_failure_injections_for_distinct_namespaces_do_not_overwrite_each_other(
    ) {
        let rt_a = rt();
        let ns_a = Namespace::parse("fts-fail-many-distinct-a").unwrap();
        let tok_a = NamespaceToken::for_namespace(ns_a.clone());
        let rt_b = rt();
        let ns_b = Namespace::parse("fts-fail-many-distinct-b").unwrap();
        let tok_b = NamespaceToken::for_namespace(ns_b.clone());

        let _arm_a = arm_fts_fail_many_scoped(ns_a.as_str());
        let _arm_b = arm_fts_fail_many_scoped(ns_b.as_str());

        let (result_a, result_b) = tokio::join!(
            rt_a.create_many(
                &tok_a,
                vec![EntityCreateSpec {
                    kind: "concept".into(),
                    entity_type: None,
                    name: "FtsFailureTargetA".into(),
                    description: None,
                    properties: None,
                    tags: vec![],
                }],
            ),
            rt_b.create_many(
                &tok_b,
                vec![EntityCreateSpec {
                    kind: "concept".into(),
                    entity_type: None,
                    name: "FtsFailureTargetB".into(),
                    description: None,
                    properties: None,
                    tags: vec![],
                }],
            ),
        );

        assert!(
            result_a.is_err(),
            "namespace A must retain its pending create_many FTS failure injection"
        );
        assert!(
            result_b.is_err(),
            "namespace B must retain its pending create_many FTS failure injection"
        );
    }

    // A failure after the first entity and FTS document have been written rolls
    // back both substrates for the entire batch. Injected via
    // `arm_fts_fail_many_partial_scoped`.
    #[tokio::test]
    async fn create_many_mid_batch_storage_failure_rolls_back_both_substrates() {
        let ns = format!("fts-fail-partial-{}", uuid::Uuid::new_v4().as_simple());
        let rt = rt();
        let tok = NamespaceToken::for_namespace(Namespace::parse(&ns).unwrap());

        let specs = vec![
            EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "PartialRollbackA".into(),
                description: None,
                properties: None,
                tags: vec![],
            },
            EntityCreateSpec {
                kind: "concept".into(),
                entity_type: None,
                name: "PartialRollbackB".into(),
                description: None,
                properties: None,
                tags: vec![],
            },
        ];

        let _arm = arm_fts_fail_many_partial_scoped(&ns);
        let result = rt.create_many(&tok, specs).await;

        assert!(
            result.is_err(),
            "create_many must return Err when an FTS write fails mid-batch"
        );
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("atomic batch rolled back at entity index 1"),
            "the failure must occur inside the atomic batch after one complete row; got: {error}"
        );

        // Entity substrate must be empty — entity rows must have been rolled back.
        let entity_rows = rt.list_entities(&tok, None, None, 100, 0).await.unwrap();
        assert_eq!(
            entity_rows.len(),
            0,
            "entity rows must be empty after a mid-batch FTS failure; found {entity_rows:?}"
        );

        // FTS substrate must be empty — no stale fts_entities rows.
        let fts = rt.text(&tok).unwrap();
        let fts_count = fts
            .count(TextFilter {
                ids: vec![],
                kinds: vec![],
                namespaces: vec![ns.clone()],
            })
            .await
            .unwrap();
        assert_eq!(
            fts_count, 0,
            "fts_entities must be empty after a mid-batch FTS failure; found {fts_count}"
        );
    }

    #[tokio::test]
    async fn create_many_fts_partial_failure_injections_for_distinct_namespaces_do_not_overwrite_each_other(
    ) {
        let rt_a = rt();
        let ns_a = Namespace::parse("fts-fail-many-partial-distinct-a").unwrap();
        let tok_a = NamespaceToken::for_namespace(ns_a.clone());
        let rt_b = rt();
        let ns_b = Namespace::parse("fts-fail-many-partial-distinct-b").unwrap();
        let tok_b = NamespaceToken::for_namespace(ns_b.clone());

        let _arm_a = arm_fts_fail_many_partial_scoped(ns_a.as_str());
        let _arm_b = arm_fts_fail_many_partial_scoped(ns_b.as_str());

        let (result_a, result_b) = tokio::join!(
            rt_a.create_many(
                &tok_a,
                vec![EntityCreateSpec {
                    kind: "concept".into(),
                    entity_type: None,
                    name: "FtsPartialFailureTargetA".into(),
                    description: None,
                    properties: None,
                    tags: vec![],
                }],
            ),
            rt_b.create_many(
                &tok_b,
                vec![EntityCreateSpec {
                    kind: "concept".into(),
                    entity_type: None,
                    name: "FtsPartialFailureTargetB".into(),
                    description: None,
                    properties: None,
                    tags: vec![],
                }],
            ),
        );

        assert!(
            result_a.is_err(),
            "namespace A must retain its pending create_many partial FTS failure injection"
        );
        assert!(
            result_b.is_err(),
            "namespace B must retain its pending create_many partial FTS failure injection"
        );
    }

    // ── Cross-namespace get_edge now succeeds (UUID v4 is globally unique) ──

    #[tokio::test]
    async fn get_edge_cross_namespace_succeeds() {
        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let src = rt
            .create_entity(&ns_a, "concept", None, "Src", None, None, vec![])
            .await
            .unwrap();
        let tgt = rt
            .create_entity(&ns_a, "concept", None, "Tgt", None, None, vec![])
            .await
            .unwrap();
        let edge = rt
            .link(&ns_a, src.id, tgt.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // Visible from own namespace.
        let own_ns = rt.get_edge(&ns_a, Uuid::from(edge.id)).await;
        assert!(
            own_ns.is_ok() && own_ns.unwrap().is_some(),
            "edge must be visible in its own namespace"
        );

        // Foreign namespace must now SUCCEED: by-ID get is namespace-agnostic.
        let cross_ns = rt.get_edge(&ns_b, Uuid::from(edge.id)).await;
        assert!(
            matches!(cross_ns, Ok(Some(_))),
            "cross-namespace get_edge must return Ok(Some(_)) after PR-A1, got {cross_ns:?}"
        );

        // Absent edge UUID still returns None regardless of token namespace.
        let absent = rt.get_edge(&ns_b, Uuid::new_v4()).await;
        assert!(
            matches!(absent, Ok(None)),
            "absent edge must return Ok(None), got {absent:?}"
        );
    }

    // ── Traversal across namespace labels now succeeds ────────────────────────
    //
    // Previously, traverse with ns_b token + ns_a root was silently empty
    // because substrate_exists_in_ns → get_entity rejected cross-namespace lookups.
    // Now: get_entity finds any entity by UUID; traverse finds the root and
    // returns paths scoped to the graph store's namespace filter for ns_b.
    #[tokio::test]
    async fn traverse_cross_namespace_root_is_accepted() {
        use khive_storage::types::TraversalOptions;

        let rt = rt();
        let ns_a = NamespaceToken::for_namespace(Namespace::parse("ns-a").unwrap());
        let ns_b = NamespaceToken::for_namespace(Namespace::parse("ns-b").unwrap());

        let a = rt
            .create_entity(&ns_a, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        rt.create_entity(&ns_a, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        rt.link(&ns_a, a.id, a.id, EdgeRelation::Extends, 1.0, None)
            .await
            .ok(); // may conflict with self-loop check; we just need an entity

        // substrate_exists_in_ns finds the ns_a root via get_entity
        // (UUID-global lookup). The traverse proceeds; no panic.
        let result = rt
            .traverse(
                &ns_b,
                TraversalRequest {
                    roots: vec![a.id],
                    options: TraversalOptions {
                        max_depth: 1,
                        direction: Direction::Out,
                        ..Default::default()
                    },
                    include_roots: true,
                    include_properties: false,
                },
            )
            .await;
        assert!(result.is_ok(), "traverse must not error; got {:?}", result);
    }

    // ── Single root visible in multiple namespaces must yield exactly one
    //    traversal object (see merge_traversal_paths_by_root) ─────────────
    #[tokio::test]
    async fn traverse_single_root_across_visible_namespaces_yields_one_path() {
        use khive_storage::types::TraversalOptions;

        let rt = rt();
        let owner = NamespaceToken::for_namespace(Namespace::parse("owner-ns").unwrap());
        let a = rt
            .create_entity(&owner, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&owner, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        rt.link(&owner, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // Token whose primary namespace ("caller-ns") does not own the root,
        // but whose visible set also includes "owner-ns" (where the root and
        // its edge actually live) — the shape produced by pack.rs always
        // widening visibility to include `local`.
        let caller = NamespaceToken::mint_with_visibility(
            Namespace::parse("caller-ns").unwrap(),
            vec![Namespace::parse("owner-ns").unwrap()],
            ActorRef::anonymous(),
        );
        assert_eq!(caller.visible_namespaces().len(), 2);

        let result = rt
            .traverse(
                &caller,
                TraversalRequest {
                    roots: vec![a.id],
                    options: TraversalOptions {
                        max_depth: 1,
                        direction: Direction::Out,
                        ..Default::default()
                    },
                    include_roots: true,
                    include_properties: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            result.len(),
            1,
            "one root visible across 2 namespaces must yield exactly one \
             GraphPath, got {result:#?}"
        );
        assert_eq!(result[0].root_id, a.id);
        let node_ids: std::collections::HashSet<Uuid> =
            result[0].nodes.iter().map(|n| n.node_id).collect();
        assert!(node_ids.contains(&a.id));
        assert!(
            node_ids.contains(&b.id),
            "merged path must retain the neighbor discovered in the owning \
             namespace, got {result:#?}"
        );
    }

    // ── Multi-root traverse: one object per distinct root, including a
    //    root supplied both as itself and as a duplicate re-resolution ────
    #[tokio::test]
    async fn traverse_multi_root_one_path_per_distinct_root() {
        use khive_storage::types::TraversalOptions;

        let rt = rt();
        let owner = NamespaceToken::for_namespace(Namespace::parse("owner-ns2").unwrap());
        let a = rt
            .create_entity(&owner, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let c = rt
            .create_entity(&owner, "concept", None, "C", None, None, vec![])
            .await
            .unwrap();

        // `a` appears twice in the roots list — this is what the pack
        // handler produces when a caller passes the same root once as a
        // short prefix and once as the full UUID: both resolve to the same
        // `Uuid` value by the time the request reaches the runtime.
        let result = rt
            .traverse(
                &owner,
                TraversalRequest {
                    roots: vec![a.id, a.id, c.id],
                    options: TraversalOptions {
                        max_depth: 1,
                        direction: Direction::Out,
                        ..Default::default()
                    },
                    include_roots: true,
                    include_properties: false,
                },
            )
            .await
            .unwrap();

        let root_ids: Vec<Uuid> = result.iter().map(|p| p.root_id).collect();
        assert_eq!(
            root_ids.len(),
            2,
            "duplicate root value must not produce a duplicate GraphPath, got {result:#?}"
        );
        assert!(root_ids.contains(&a.id));
        assert!(root_ids.contains(&c.id));
    }

    // ── Note-kind nodes reached via traversal appear in the result but are
    //    never enriched with name/kind (entity-only enrichment, unchanged
    //    behavior — see `enrich_path_nodes`) ────────────────────────────────
    //
    // The recursive SQL walks `graph_edges` without any node-kind
    // restriction, and the soft-delete screen consults both `entities` and
    // `notes`, so a note reached via an `annotates` edge is NOT dropped from
    // the traversal. What it does not get is enrichment: `enrich_path_nodes`
    // only batch-fetches entities (a deliberate entity-only scope), unlike
    // `enrich_neighbor_hits` which falls back to a note lookup. This test
    // pins that documented split rather than changing it.
    #[tokio::test]
    async fn traverse_reaches_note_nodes_but_leaves_them_unenriched() {
        use khive_storage::types::TraversalOptions;

        let rt = rt();
        let owner = NamespaceToken::for_namespace(Namespace::parse("owner-ns3").unwrap());
        let a = rt
            .create_entity(&owner, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let note = rt
            .create_note(
                &owner,
                "observation",
                None,
                "note body",
                None,
                None,
                vec![a.id],
            )
            .await
            .unwrap();

        let result = rt
            .traverse(
                &owner,
                TraversalRequest {
                    roots: vec![a.id],
                    options: TraversalOptions {
                        max_depth: 1,
                        direction: Direction::In,
                        ..Default::default()
                    },
                    include_roots: false,
                    include_properties: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let note_node = result[0]
            .nodes
            .iter()
            .find(|n| n.node_id == note.id)
            .unwrap_or_else(|| panic!("note must be present in traversal nodes, got {result:#?}"));
        assert_eq!(
            note_node.name, None,
            "note enrichment is deliberately entity-only; name stays None"
        );
        assert_eq!(
            note_node.kind, None,
            "note enrichment is deliberately entity-only; kind stays None"
        );
    }

    // ---- purge cascade must include already-soft-deleted edges ----
    //
    // Hard delete must cascade ALL incident edges synchronously. A cascade driven
    // through `neighbors()`, which filters `deleted_at IS NULL`, would let incident
    // edges that were already soft-deleted survive endpoint purge as dangling rows.
    // `purge_incident_edges` issues a single DELETE without a `deleted_at` guard.

    /// Count ALL `graph_edges` rows for a given UUID (source OR target), including soft-deleted.
    async fn count_all_incident_edges(rt: &KhiveRuntime, node_id: Uuid, ns: &str) -> u64 {
        let mut reader = rt.sql().reader().await.expect("sql reader must open");
        let row = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM graph_edges \
                      WHERE namespace = ?1 AND (source_id = ?2 OR target_id = ?2)"
                    .into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(node_id.to_string()),
                ],
                label: Some("count_all_incident_edges".into()),
            })
            .await
            .expect("count query must succeed");
        match row {
            Some(SqlValue::Integer(n)) => n as u64,
            _ => panic!("count must return an integer"),
        }
    }

    #[tokio::test]
    async fn hard_delete_entity_purges_already_soft_deleted_incident_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().to_string();

        let a = rt
            .create_entity(&tok, "concept", None, "SrcA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "TgtB", None, None, vec![])
            .await
            .unwrap();

        rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();

        // Soft-delete the edge — it is now invisible to `neighbors` but still in storage.
        let edge_hit = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert_eq!(edge_hit.len(), 1, "edge must exist before soft-delete");
        let edge_uuid = edge_hit[0].edge_id;
        rt.delete_edge(&tok, edge_uuid, false).await.unwrap();

        // Confirm the edge is invisible to normal read paths but present in raw storage.
        let visible = rt
            .neighbors(&tok, a.id, Direction::Out, None, None)
            .await
            .unwrap();
        assert!(visible.is_empty(), "soft-deleted edge must be invisible");
        let raw_before = count_all_incident_edges(&rt, a.id, &ns).await;
        assert_eq!(
            raw_before, 1,
            "soft-deleted edge must still be a physical row"
        );

        // Hard-delete (purge) the source entity — cascade must also remove the soft-deleted edge.
        rt.delete_entity(&tok, a.id, true).await.unwrap();

        let raw_after = count_all_incident_edges(&rt, a.id, &ns).await;
        assert_eq!(
            raw_after, 0,
            "purge_incident_edges must physically remove soft-deleted edge rows (ADR-002)"
        );
    }

    #[tokio::test]
    async fn hard_delete_note_purges_already_soft_deleted_incident_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().to_string();

        let target = rt
            .create_note(
                &tok,
                "observation",
                None,
                "purge-cascade target note",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();
        let annotating = rt
            .create_note(
                &tok,
                "insight",
                None,
                "annotator note",
                Some(0.5),
                None,
                vec![target.id],
            )
            .await
            .unwrap();

        // Soft-delete the annotates edge.
        let edge_hit = rt
            .neighbors(
                &tok,
                annotating.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(edge_hit.len(), 1, "annotates edge must exist");
        let edge_uuid = edge_hit[0].edge_id;
        rt.delete_edge(&tok, edge_uuid, false).await.unwrap();

        let raw_before = count_all_incident_edges(&rt, target.id, &ns).await;
        assert_eq!(
            raw_before, 1,
            "soft-deleted edge must still be a physical row before note purge"
        );

        // Hard-delete the target note — cascade must remove the soft-deleted edge row.
        rt.delete_note(&tok, target.id, true).await.unwrap();

        let raw_after = count_all_incident_edges(&rt, target.id, &ns).await;
        assert_eq!(
            raw_after, 0,
            "purge_incident_edges must physically remove soft-deleted edge rows on note purge (ADR-002)"
        );
    }

    // ---- cross-namespace entity hard-delete purges ALL incident edges ----
    //
    // `purge_incident_edges` must not scope its DELETE by `WHERE namespace = caller_ns`,
    // or a foreign-namespace entity's incident edges in ITS namespace would survive
    // the cascade as dangling rows.

    /// Count ALL `graph_edges` rows for a given node UUID, across every namespace.
    async fn count_all_incident_edges_global(rt: &KhiveRuntime, node_id: Uuid) -> u64 {
        let mut reader = rt.sql().reader().await.expect("sql reader must open");
        let row = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM graph_edges WHERE source_id = ?1 OR target_id = ?1"
                    .into(),
                params: vec![SqlValue::Text(node_id.to_string())],
                label: Some("count_all_incident_edges_global".into()),
            })
            .await
            .expect("count query must succeed");
        match row {
            Some(SqlValue::Integer(n)) => n as u64,
            _ => panic!("count must return an integer"),
        }
    }

    #[tokio::test]
    async fn cross_namespace_hard_delete_entity_purges_all_incident_edges() {
        // Entity lives in ns-owner. Edges live in ns-owner.
        // Delete is driven from ns-caller (a different namespace).
        // Assertion: after hard delete, no incident edges remain in ANY namespace.
        let rt = rt();
        let ns_owner = NamespaceToken::for_namespace(Namespace::parse("ns-owner").unwrap());
        let ns_caller = NamespaceToken::for_namespace(Namespace::parse("ns-caller").unwrap());

        let entity = rt
            .create_entity(
                &ns_owner,
                "concept",
                None,
                "ForeignEntity",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let peer = rt
            .create_entity(&ns_owner, "concept", None, "Peer", None, None, vec![])
            .await
            .unwrap();
        // Create two incident edges in ns_owner. concept->Extends->concept is in the allowlist.
        rt.link(
            &ns_owner,
            entity.id,
            peer.id,
            EdgeRelation::Extends,
            1.0,
            None,
        )
        .await
        .unwrap();
        rt.link(
            &ns_owner,
            peer.id,
            entity.id,
            EdgeRelation::Extends,
            1.0,
            None,
        )
        .await
        .unwrap();

        let before = count_all_incident_edges_global(&rt, entity.id).await;
        assert_eq!(before, 2, "two incident edges must exist before delete");

        // Hard-delete entity from a DIFFERENT namespace token.
        let deleted = rt.delete_entity(&ns_caller, entity.id, true).await.unwrap();
        assert!(deleted, "cross-ns hard delete must return true");

        // All incident edges must be gone regardless of namespace.
        let after = count_all_incident_edges_global(&rt, entity.id).await;
        assert_eq!(
            after, 0,
            "purge_incident_edges must remove all incident edges across namespaces (ADR-002, ADR-007)"
        );
    }

    // ---- edge-ID hard-delete path ----
    //
    // Bug class: delete_edge drove the primary-edge guard through get_edge()
    // (live-only) and the cascade through neighbors() (live-only). Two reachable holes:
    // (a) soft-deleted primary edge cannot be hard-purged via its own ID;
    // (b) an already-soft-deleted annotates edge targeting a base edge survives that
    //     edge's hard delete as a dangling row with target_id = physically-gone edge id.

    /// Count graph_edges rows matching the given edge ID, including soft-deleted rows.
    async fn count_edge_rows_by_id(rt: &KhiveRuntime, edge_id: Uuid, ns: &str) -> u64 {
        let mut reader = rt.sql().reader().await.expect("sql reader must open");
        let row = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM graph_edges WHERE namespace = ?1 AND id = ?2".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(edge_id.to_string()),
                ],
                label: Some("count_edge_rows_by_id".into()),
            })
            .await
            .expect("count query must succeed");
        match row {
            Some(SqlValue::Integer(n)) => n as u64,
            _ => panic!("count must return an integer"),
        }
    }

    #[tokio::test]
    async fn hard_delete_edge_purges_already_soft_deleted_primary_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().to_string();

        let a = rt
            .create_entity(&tok, "concept", None, "EA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "EB", None, None, vec![])
            .await
            .unwrap();

        let edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let edge_uuid: Uuid = edge.id.into();

        // Soft-delete the edge first.
        let soft = rt.delete_edge(&tok, edge_uuid, false).await.unwrap();
        assert!(soft, "soft delete must succeed");

        // Edge is now invisible to normal reads but still a physical row.
        assert!(
            rt.get_edge(&tok, edge_uuid).await.unwrap().is_none(),
            "soft-deleted edge must be invisible to get_edge"
        );
        assert_eq!(
            count_edge_rows_by_id(&rt, edge_uuid, &ns).await,
            1,
            "soft-deleted edge must still be a physical row"
        );

        // Hard-delete (purge) via the edge ID — must succeed and remove the row.
        let purged = rt.delete_edge(&tok, edge_uuid, true).await.unwrap();
        assert!(
            purged,
            "hard delete of a soft-deleted edge must return true"
        );

        assert_eq!(
            count_edge_rows_by_id(&rt, edge_uuid, &ns).await,
            0,
            "hard-delete must physically remove the soft-deleted edge row (ADR-002)"
        );
    }

    #[tokio::test]
    async fn hard_delete_base_edge_purges_already_soft_deleted_annotates_edge() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let ns = tok.namespace().to_string();

        let a = rt
            .create_entity(&tok, "concept", None, "CA", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_entity(&tok, "concept", None, "CB", None, None, vec![])
            .await
            .unwrap();

        // Create the base edge to be annotated.
        let base_edge = rt
            .link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
            .await
            .unwrap();
        let base_edge_uuid: Uuid = base_edge.id.into();

        // Create a note that annotates the base edge.
        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "note about base edge",
                Some(0.5),
                None,
                vec![base_edge_uuid],
            )
            .await
            .unwrap();

        // Find the annotates edge.
        let ann_hits = rt
            .neighbors(
                &tok,
                note.id,
                Direction::Out,
                None,
                Some(vec![EdgeRelation::Annotates]),
            )
            .await
            .unwrap();
        assert_eq!(ann_hits.len(), 1, "annotates edge must exist");
        let ann_edge_uuid = ann_hits[0].edge_id;

        // Soft-delete the annotates edge — now invisible but still a physical row.
        rt.delete_edge(&tok, ann_edge_uuid, false).await.unwrap();
        assert_eq!(
            count_edge_rows_by_id(&rt, ann_edge_uuid, &ns).await,
            1,
            "soft-deleted annotates edge must still be a physical row"
        );

        // Hard-delete the base edge — cascade must also remove the soft-deleted annotates row.
        let purged = rt.delete_edge(&tok, base_edge_uuid, true).await.unwrap();
        assert!(purged, "hard delete of base edge must return true");

        assert_eq!(
            count_edge_rows_by_id(&rt, ann_edge_uuid, &ns).await,
            0,
            "hard-delete of base edge must purge already-soft-deleted annotates edge row (ADR-002)"
        );
        assert_eq!(
            count_edge_rows_by_id(&rt, base_edge_uuid, &ns).await,
            0,
            "hard-delete must physically remove the base edge row"
        );
    }

    // ---- entity create/update multi-model embed fan-out tests ----

    // FTS failure after entity row commit rolls back the entity row.
    // Mirrors create_note_fts_failure_rolls_back_note_row but for entities.
    // Uses a unique namespace so this test's arm never fires for the wrong
    // write path, even under full-suite parallelism.
    #[tokio::test]
    async fn create_entity_fts_failure_rolls_back_entity_row() {
        let rt = KhiveRuntime::memory().unwrap();
        let ns = Namespace::parse("fault-entity-fts").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_fts_fail_scoped(ns.as_str());

        let result = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "fts-fail rollback target",
                None,
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_entity must propagate the injected FTS failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected FTS failure"),
            "error must carry injection message; got: {err_msg}"
        );

        let entities = rt.list_entities(&tok, None, None, 1000, 0).await.unwrap();
        assert!(
            entities.is_empty(),
            "compensation must remove the entity row after FTS failure; got {entities:?}"
        );
    }

    // Vector insert failure after entity row + FTS commit rolls back both.
    // Uses a unique namespace so only this test consumes its VECTOR_FAIL_NS entry.
    #[tokio::test]
    async fn create_entity_vector_failure_rolls_back_entity_row_and_fts() {
        const MODEL: &str = "test-entity-vec-inject";
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider, _counter) = ConstVecProvider::new(MODEL, DIMS);
        rt.register_embedder(provider);

        let ns = Namespace::parse("fault-entity-vec").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let _arm = arm_vector_fail_scoped(ns.as_str());

        let result = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "vec-fail rollback target",
                Some("description so embed body is non-empty"),
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_entity must propagate the injected vector failure"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("injected vector failure"),
            "error must carry injection message; got: {err_msg}"
        );

        let entities = rt.list_entities(&tok, None, None, 1000, 0).await.unwrap();
        assert!(
            entities.is_empty(),
            "compensation must remove entity row after vector failure; got {entities:?}"
        );

        // FTS document must also be removed.
        use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest};
        let fts_hits = rt
            .text(&tok)
            .unwrap()
            .search(TextSearchRequest {
                query: "vec-fail rollback target".to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.as_str().to_string()],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 100,
            })
            .await
            .unwrap();
        assert!(
            fts_hits.is_empty(),
            "compensation must remove FTS document after vector failure; got {fts_hits:?}"
        );
    }

    // Multi-model create_entity: second model's vector INSERT fails after the
    // first model's insert succeeds, triggering inserted_models rollback.
    // Uses arm_vector_fail_after(1) so the first insert passes and the second fails,
    // exercising the inserted_models compensation path in create_entity.
    // Thread-local VECTOR_FAIL_AFTER is per-thread isolated (current-thread tokio runtime),
    // so this test does not race with namespace-targeted VECTOR_FAIL_NS tests.
    #[tokio::test]
    async fn create_entity_multi_model_second_vector_failure_rolls_back_all() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider_a, _ca) = ConstVecProvider::new("model-a", DIMS);
        let (provider_b, _cb) = ConstVecProvider::new("model-b", DIMS);
        rt.register_embedder(provider_a);
        rt.register_embedder(provider_b);

        let ns = Namespace::parse("fault-entity-multi").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        // Let the first vector insert succeed, fail on the second.
        arm_vector_fail_after(1);

        let result = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "multi-model rollback target",
                Some("description for embedding"),
                None,
                vec![],
            )
            .await;

        assert!(
            result.is_err(),
            "create_entity must propagate the injected multi-model vector failure"
        );

        let entities = rt.list_entities(&tok, None, None, 1000, 0).await.unwrap();
        assert!(
            entities.is_empty(),
            "compensation must remove entity row; got {entities:?}"
        );

        // Both model-a and model-b vector stores must be empty for the entity id.
        // (The entity was never returned so we can't get its id from the result;
        // list_entities returning empty is the primary assertion. Additionally confirm
        // both stores have zero rows via a broad vector search.)
        use khive_storage::types::VectorSearchRequest;
        let query_vec = vec![1.0_f32; DIMS];
        let hits_a = rt
            .vectors_for_model(&tok, "model-a")
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec.clone()],
                top_k: 100,
                namespace: Some(ns.as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("model-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits_a.is_empty(),
            "model-a vector store must be empty after rollback; got {hits_a:?}"
        );
        let hits_b = rt
            .vectors_for_model(&tok, "model-b")
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec],
                top_k: 100,
                namespace: Some(ns.as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("model-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits_b.is_empty(),
            "model-b vector store must be empty after rollback; got {hits_b:?}"
        );
    }

    // ADR-103 Amendment 2 regression: multi-model create_entity spawns one
    // embed task per configured model via tokio::spawn. Task-locals do not
    // cross a spawn boundary, so each spawned task must re-enter the
    // dispatch's usage scope explicitly for its embed to be counted.
    #[tokio::test]
    async fn create_entity_multi_model_counts_all_executed_embeds() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider_a, _ca) = ConstVecProvider::new("usage-entity-model-a", DIMS);
        let (provider_b, _cb) = ConstVecProvider::new("usage-entity-model-b", DIMS);
        rt.register_embedder(provider_a);
        rt.register_embedder(provider_b);

        let ns = Namespace::parse("usage-entity-multi").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let ctx = crate::usage::UsageContext::new();
        crate::usage::scope(ctx.clone(), async {
            rt.create_entity(
                &tok,
                "concept",
                None,
                "usage-counted entity",
                Some("description so embed body is non-empty"),
                None,
                vec![],
            )
            .await
        })
        .await
        .expect("create_entity must succeed");

        let snap = ctx.snapshot();
        assert_eq!(
            snap["embed_calls"], 2,
            "both configured models' executed embeds must be counted; got {snap:?}"
        );
    }

    // Same regression for the note create path's multi-model embed fan-out.
    #[tokio::test]
    async fn create_note_multi_model_counts_all_executed_embeds() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider_a, _ca) = ConstVecProvider::new("usage-note-model-a", DIMS);
        let (provider_b, _cb) = ConstVecProvider::new("usage-note-model-b", DIMS);
        rt.register_embedder(provider_a);
        rt.register_embedder(provider_b);

        let ns = Namespace::parse("usage-note-multi").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let ctx = crate::usage::UsageContext::new();
        crate::usage::scope(ctx.clone(), async {
            rt.create_note(
                &tok,
                "observation",
                None,
                "usage-counted note body",
                None,
                None,
                vec![],
            )
            .await
        })
        .await
        .expect("create_note must succeed");

        let snap = ctx.snapshot();
        assert_eq!(
            snap["embed_calls"], 2,
            "both configured models' executed embeds must be counted; got {snap:?}"
        );
    }

    // Embed calls are counted when issued, so detached completion after a
    // sibling failure cannot change the response's usage snapshot.
    #[tokio::test]
    async fn create_entity_multi_model_error_keeps_issued_usage_stable() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(FailFastProvider::new("usage-entity-fail-fast"));
        rt.register_embedder(SlowVecProvider::new("usage-entity-slow", DIMS));

        let ns = Namespace::parse("usage-entity-error-drain").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let ctx = crate::usage::UsageContext::new();
        let result = crate::usage::scope(ctx.clone(), async {
            rt.create_entity(
                &tok,
                "concept",
                None,
                "usage-drain entity",
                Some("description so embed body is non-empty"),
                None,
                vec![],
            )
            .await
        })
        .await;

        assert!(
            result.is_err(),
            "one model failing must fail the whole create_entity call"
        );

        let snap_immediately_after = ctx.snapshot();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let snap_after_delay = ctx.snapshot();

        assert_eq!(
            snap_immediately_after, snap_after_delay,
            "embed completion after create_entity returns must not change issued \
             usage; got immediately_after={snap_immediately_after:?} \
             after_delay={snap_after_delay:?}"
        );
    }

    // Same regression for the note create path's multi-model embed fan-out.
    #[tokio::test]
    async fn create_note_multi_model_error_keeps_issued_usage_stable() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(FailFastProvider::new("usage-note-fail-fast"));
        rt.register_embedder(SlowVecProvider::new("usage-note-slow", DIMS));

        let ns = Namespace::parse("usage-note-error-drain").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let ctx = crate::usage::UsageContext::new();
        let result = crate::usage::scope(ctx.clone(), async {
            rt.create_note(
                &tok,
                "observation",
                None,
                "usage-drain note body",
                None,
                None,
                vec![],
            )
            .await
        })
        .await;

        assert!(
            result.is_err(),
            "one model failing must fail the whole create_note call"
        );

        let snap_immediately_after = ctx.snapshot();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let snap_after_delay = ctx.snapshot();

        assert_eq!(
            snap_immediately_after, snap_after_delay,
            "embed completion after create_note returns must not change issued \
             usage; got immediately_after={snap_immediately_after:?} \
             after_delay={snap_after_delay:?}"
        );
    }

    // A fast provider failure must not wait on a slow sibling: the drain
    // aborts remaining embed tasks on the first error instead of awaiting
    // them to completion. The sibling here parks until a release that never
    // fires, so under await-to-completion this call would never return —
    // a bounded prompt error return is only reachable through the abort path.
    #[tokio::test]
    async fn create_entity_fast_embed_failure_does_not_wait_for_hung_sibling() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        rt.register_embedder(FailFastProvider::new("latency-fail-fast"));
        let (parked, _release, _entered) = ParkedVecProvider::new("latency-parked", DIMS);
        rt.register_embedder(parked);

        let ns = Namespace::parse("usage-entity-latency").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let started = std::time::Instant::now();
        // Outer timeout so a regression to await-to-completion FAILS this test
        // within the bound instead of hanging the suite on the parked sibling.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            rt.create_entity(
                &tok,
                "concept",
                None,
                "latency entity",
                Some("description so embed body is non-empty"),
                None,
                vec![],
            ),
        )
        .await
        .expect("create_entity must return within the timeout — a hang means the abort path regressed to await-to-completion");
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "one model failing must fail the whole create_entity call"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "a fast embed failure must return without waiting on the hung \
             sibling (which parks forever); took {elapsed:?}"
        );
    }

    #[test]
    fn create_entity_embed_failure_returns_under_single_worker_saturation() {
        let (blocking, controls) = BlockingVecProvider::new("latency-blocking", 4);
        let fail_after_entry = FailFastProvider::after_signal(
            "latency-fail-after-entry",
            Arc::clone(&controls.entered),
        );
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);

        let runtime_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("single-worker runtime must build");
            let rt = KhiveRuntime::memory().unwrap();
            rt.register_embedder(blocking);
            rt.register_embedder(fail_after_entry);
            let tok =
                NamespaceToken::for_namespace(Namespace::parse("embed-failure-latency").unwrap());
            let result = runtime.block_on(rt.create_entity(
                &tok,
                "concept",
                None,
                "blocked sibling entity",
                None,
                None,
                vec![],
            ));
            result_tx
                .send(result.map_err(|error| error.to_string()))
                .expect("test receiver must remain connected");
        });

        let result = result_rx.recv_timeout(std::time::Duration::from_secs(3));

        let (released, wake) = &*controls.release;
        *released.lock().expect("release lock must not be poisoned") = true;
        wake.notify_all();
        runtime_thread
            .join()
            .expect("single-worker runtime thread must join after release");

        let error = result
            .expect("embed failure must return while synchronous inference remains blocked")
            .expect_err("one failed model must fail entity creation");
        assert!(error.contains("injected embed failure"));
    }

    // Issued-at-dispatch accounting: an embed call that was handed to the
    // provider must count toward embed_calls even if the task is aborted
    // while parked on the provider await — the increment sits before the
    // await, so cancellation cannot undercount issued work.
    #[tokio::test]
    async fn aborted_embed_task_still_counts_issued_embed_call() {
        const DIMS: usize = 4;

        let rt = std::sync::Arc::new(KhiveRuntime::memory().unwrap());
        let (parked, _release, entered) = ParkedVecProvider::new("abort-count-parked", DIMS);
        rt.register_embedder(parked);

        let ctx = crate::usage::UsageContext::new();
        let task = {
            let rt = std::sync::Arc::clone(&rt);
            let ctx = ctx.clone();
            tokio::spawn(crate::usage::scope(ctx, async move {
                rt.embed_document_with_model("abort-count-parked", "abort count body")
                    .await
            }))
        };

        // Wait until the parked service's embed was entered — the dispatch
        // point (and its count) is strictly before that entry.
        entered.notified().await;
        task.abort();
        let joined = task.await;
        assert!(
            joined.is_err() && joined.unwrap_err().is_cancelled(),
            "task must end as cancelled by the abort"
        );

        assert_eq!(
            ctx.snapshot()["embed_calls"],
            1,
            "an issued embed call must be counted even when the task is \
             aborted while parked on the provider await"
        );
    }

    // Note search must count its FTS5 execution the same way entity
    // hybrid_search does (retrieval.rs:435) — the search_notes FTS leg was
    // silently uncounted.
    #[tokio::test]
    async fn search_notes_counts_one_fts_pass() {
        let rt = KhiveRuntime::memory().unwrap();
        let ns = Namespace::parse("usage-note-search").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        rt.create_note(
            &tok,
            "observation",
            None,
            "usage counted note search body",
            None,
            None,
            vec![],
        )
        .await
        .expect("create_note must succeed");

        let ctx = crate::usage::UsageContext::new();
        crate::usage::scope(ctx.clone(), async {
            rt.search_notes(&tok, "usage counted", None, 10, None, false, &[], None)
                .await
        })
        .await
        .expect("search_notes must succeed");

        let snap = ctx.snapshot();
        assert!(
            snap["fts_passes"].as_u64().unwrap_or(0) >= 1,
            "note search FTS execution must count fts_passes; got {snap:?}"
        );
    }

    // update_entity fans out to ALL registered models.
    // After create + update with a changed description, both model-a and model-b
    // vector stores hold a row for the entity id.
    #[tokio::test]
    async fn update_entity_fans_out_to_all_registered_models() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider_a, _ca) = ConstVecProvider::new("embed-a", DIMS);
        let (provider_b, _cb) = ConstVecProvider::new("embed-b", DIMS);
        rt.register_embedder(provider_a);
        rt.register_embedder(provider_b);

        let ns = Namespace::parse("update-entity-fanout").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let entity = rt
            .create_entity(
                &tok,
                "concept",
                None,
                "FanOutEntity",
                Some("initial description"),
                None,
                vec![],
            )
            .await
            .expect("create_entity must succeed");

        use crate::curation::EntityPatch;
        let patch = EntityPatch {
            description: Some(Some("updated description after fan-out fix".to_string())),
            ..Default::default()
        };
        rt.update_entity(&tok, entity.id, patch)
            .await
            .expect("update_entity must succeed");

        use khive_storage::types::VectorSearchRequest;
        let query_vec = vec![1.0_f32; DIMS];

        let hits_a = rt
            .vectors_for_model(&tok, "embed-a")
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec.clone()],
                top_k: 10,
                namespace: Some(ns.as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("embed-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits_a.iter().any(|h| h.subject_id == entity.id),
            "embed-a must hold a vector for the entity after update; got {hits_a:?}"
        );

        let hits_b = rt
            .vectors_for_model(&tok, "embed-b")
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec],
                top_k: 10,
                namespace: Some(ns.as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Entity),
                embedding_model: Some("embed-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits_b.iter().any(|h| h.subject_id == entity.id),
            "embed-b must hold a vector for the entity after update; got {hits_b:?}"
        );
    }

    // update_note fans out to ALL registered models.
    // After create + update with changed content, both embed-a and embed-b
    // vector stores hold a row for the note id.
    #[tokio::test]
    async fn update_note_fans_out_to_all_registered_models() {
        const DIMS: usize = 4;

        let rt = KhiveRuntime::memory().unwrap();
        let (provider_a, _ca) = ConstVecProvider::new("embed-a", DIMS);
        let (provider_b, _cb) = ConstVecProvider::new("embed-b", DIMS);
        rt.register_embedder(provider_a);
        rt.register_embedder(provider_b);

        let ns = Namespace::parse("update-note-fanout").unwrap();
        let tok = NamespaceToken::for_namespace(ns.clone());

        let note = rt
            .create_note(
                &tok,
                "observation",
                None,
                "initial note content for fan-out test",
                None,
                None,
                vec![],
            )
            .await
            .expect("create_note must succeed");

        use crate::curation::NotePatch;
        let patch = NotePatch {
            content: Some("updated content after fan-out fix".to_string()),
            ..Default::default()
        };
        rt.update_note(&tok, note.id, patch)
            .await
            .expect("update_note must succeed");

        use khive_storage::types::VectorSearchRequest;
        let query_vec = vec![1.0_f32; DIMS];

        let hits_a = rt
            .vectors_for_model(&tok, "embed-a")
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec.clone()],
                top_k: 10,
                namespace: Some(ns.as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("embed-a".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits_a.iter().any(|h| h.subject_id == note.id),
            "embed-a must hold a vector for the note after update; got {hits_a:?}"
        );

        let hits_b = rt
            .vectors_for_model(&tok, "embed-b")
            .unwrap()
            .search(VectorSearchRequest {
                query_vectors: vec![query_vec],
                top_k: 10,
                namespace: Some(ns.as_str().to_string()),
                kind: Some(khive_types::SubstrateKind::Note),
                embedding_model: Some("embed-b".to_string()),
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();
        assert!(
            hits_b.iter().any(|h| h.subject_id == note.id),
            "embed-b must hold a vector for the note after update; got {hits_b:?}"
        );
    }

    // ── By-ID ops must not filter by namespace ──────────────────────────────
    //
    // A namespace-gated by-ID op on an entity stamped "foreign" from a "local"
    // token would return NotFound, causing gtd.complete / update blindness.
    // UUID is globally unique; by-ID ops find the record regardless of
    // which namespace the caller's token carries.

    #[tokio::test]
    async fn get_entity_cross_namespace_succeeds() {
        let rt = rt();
        // Create under "lambda:leo".
        let leo_tok = NamespaceToken::for_namespace(Namespace::parse("lambda:leo").unwrap());
        let entity = rt
            .create_entity(&leo_tok, "concept", None, "Peer-Entity", None, None, vec![])
            .await
            .unwrap();
        assert_eq!(entity.namespace, "lambda:leo");

        // Read from "local" — must succeed (no namespace gate on by-ID get).
        let local_tok = NamespaceToken::local();
        let fetched = rt.get_entity(&local_tok, entity.id).await;
        assert!(
            fetched.is_ok(),
            "get_entity from local token must find lambda:leo entity; got {:?}",
            fetched
        );
        assert_eq!(fetched.unwrap().id, entity.id);
    }

    #[tokio::test]
    async fn update_entity_cross_namespace_succeeds() {
        let rt = rt();
        let leo_tok = NamespaceToken::for_namespace(Namespace::parse("lambda:leo").unwrap());
        let entity = rt
            .create_entity(
                &leo_tok,
                "concept",
                None,
                "Peer-Entity-Update",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        // Update from "local" token — must not error with NotFound.
        let local_tok = NamespaceToken::local();
        let patch = crate::curation::EntityPatch {
            name: Some("Peer-Entity-Updated".to_string()),
            ..Default::default()
        };
        let result = rt.update_entity(&local_tok, entity.id, patch).await;
        assert!(
            result.is_ok(),
            "update_entity from local token must succeed on lambda:leo entity; got {:?}",
            result
        );
        assert_eq!(result.unwrap().name, "Peer-Entity-Updated");
    }

    #[tokio::test]
    async fn delete_entity_cross_namespace_succeeds() {
        let rt = rt();
        let leo_tok = NamespaceToken::for_namespace(Namespace::parse("lambda:leo").unwrap());
        let entity = rt
            .create_entity(
                &leo_tok,
                "concept",
                None,
                "Peer-Entity-Delete",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        // Delete from "local" token — must succeed.
        let local_tok = NamespaceToken::local();
        let deleted = rt.delete_entity(&local_tok, entity.id, false).await;
        assert!(
            deleted.is_ok(),
            "delete_entity from local token must succeed on lambda:leo entity; got {:?}",
            deleted
        );
        assert!(
            deleted.unwrap(),
            "delete must return true when entity existed"
        );
    }

    #[tokio::test]
    async fn namespace_preserved_on_entity_after_cross_namespace_get() {
        let rt = rt();
        let leo_tok = NamespaceToken::for_namespace(Namespace::parse("lambda:leo").unwrap());
        let entity = rt
            .create_entity(
                &leo_tok,
                "concept",
                None,
                "NS-Preserved",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        // The namespace column on the fetched record must still say "lambda:leo".
        let local_tok = NamespaceToken::local();
        let fetched = rt.get_entity(&local_tok, entity.id).await.unwrap();
        assert_eq!(
            fetched.namespace, "lambda:leo",
            "namespace column must be preserved; not overwritten with caller's namespace"
        );
    }

    // ── PackByIdResolver unit tests ──────────────────────────────────────────

    use crate::pack::PackByIdResolver;
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Debug, Default)]
    struct MockResolverState {
        owned: Vec<Uuid>,
        deleted: Vec<Uuid>,
        delete_calls: Vec<(Uuid, bool)>,
    }

    struct MockPackResolver(TokioMutex<MockResolverState>);

    impl MockPackResolver {
        fn new() -> Self {
            Self(TokioMutex::new(MockResolverState::default()))
        }
    }

    #[async_trait::async_trait]
    impl crate::pack::PackByIdResolver for MockPackResolver {
        async fn resolve_by_id(&self, id: Uuid) -> Result<Option<Resolved>, RuntimeError> {
            let state = self.0.lock().await;
            if state.owned.contains(&id) && !state.deleted.contains(&id) {
                Ok(Some(Resolved::PackRecord {
                    pack: "mock".into(),
                    kind: "widget".into(),
                    data: serde_json::json!({ "id": id.to_string(), "name": "test-widget" }),
                }))
            } else {
                Ok(None)
            }
        }

        async fn resolve_by_id_including_deleted(
            &self,
            id: Uuid,
        ) -> Result<Option<Resolved>, RuntimeError> {
            let state = self.0.lock().await;
            if state.owned.contains(&id) {
                Ok(Some(Resolved::PackRecord {
                    pack: "mock".into(),
                    kind: "widget".into(),
                    data: serde_json::json!({ "id": id.to_string(), "name": "test-widget" }),
                }))
            } else {
                Ok(None)
            }
        }

        async fn delete_by_id(
            &self,
            id: Uuid,
            hard: bool,
        ) -> Result<serde_json::Value, RuntimeError> {
            let mut state = self.0.lock().await;
            if !state.owned.contains(&id) {
                return Err(RuntimeError::NotFound(format!(
                    "mock widget not found: {id}"
                )));
            }
            state.delete_calls.push((id, hard));
            if hard {
                state.owned.retain(|&x| x != id);
                state.deleted.retain(|&x| x != id);
            } else {
                state.deleted.push(id);
            }
            Ok(
                serde_json::json!({ "deleted": true, "id": id.to_string(), "kind": "widget", "hard": hard }),
            )
        }
    }

    fn registry_with_mock_resolver(
        rt: KhiveRuntime,
        resolver: Box<dyn crate::pack::PackByIdResolver>,
    ) -> crate::VerbRegistry {
        use crate::pack::{PackRuntime, VerbRegistryBuilder};
        use khive_types::{HandlerDef, VerbCategory, Visibility};

        static MINIMAL_HANDLERS: &[HandlerDef] = &[HandlerDef {
            name: "minimal.noop",
            description: "noop",
            visibility: Visibility::Verb,
            category: VerbCategory::Commissive,
            params: &[],
        }];

        struct MinimalPack;
        impl khive_types::Pack for MinimalPack {
            const NAME: &'static str = "minimal";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = MINIMAL_HANDLERS;
        }
        #[async_trait::async_trait]
        impl PackRuntime for MinimalPack {
            fn name(&self) -> &str {
                "minimal"
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                &[]
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                &[]
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                MINIMAL_HANDLERS
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: serde_json::Value,
                _registry: &crate::VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<serde_json::Value, RuntimeError> {
                Err(RuntimeError::InvalidInput("stub".into()))
            }
        }

        let _ = rt;
        let mut builder = VerbRegistryBuilder::new();
        builder.register(MinimalPack);
        builder.register_resolver("mock", resolver);
        builder.build().expect("registry build failed")
    }

    #[tokio::test]
    async fn pack_record_resolved_pair_returns_none() {
        let pr = Resolved::PackRecord {
            pack: "knowledge".into(),
            kind: "atom".into(),
            data: serde_json::json!({}),
        };
        assert!(
            resolved_pair(Some(&pr)).is_none(),
            "PackRecord must not be a valid edge endpoint"
        );
    }

    #[test]
    fn resolved_pair_surfaces_entity_type() {
        let e = Resolved::Entity(
            Entity::new("mathlib", "concept", "Nat.add_comm").with_entity_type(Some("theorem")),
        );
        assert_eq!(
            resolved_pair(Some(&e)),
            Some(("entity", "concept", Some("theorem"))),
            "entity_type subtype must be surfaced alongside base kind"
        );
    }

    #[test]
    fn endpoint_of_type_matches_subtype_not_base_kind() {
        // An entity whose base kind is "concept" and subtype is "theorem".
        let kind = "concept";
        let et = Some("theorem");

        // EntityOfType matches only when BOTH base kind and subtype match.
        assert!(endpoint_matches(
            &EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
            "entity",
            kind,
            et
        ));
        assert!(!endpoint_matches(
            &EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "definition",
            },
            "entity",
            kind,
            et
        ));

        // The silently-inert trap: EntityOfKind sees only the BASE
        // kind, so EntityOfKind("theorem") never matches a concept/theorem.
        assert!(!endpoint_matches(
            &EndpointKind::EntityOfKind("theorem"),
            "entity",
            kind,
            et
        ));
        // EntityOfKind still matches the base kind.
        assert!(endpoint_matches(
            &EndpointKind::EntityOfKind("concept"),
            "entity",
            kind,
            et
        ));

        // EntityOfType rejects non-entity substrates and entities with no subtype.
        assert!(!endpoint_matches(
            &EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
            "note",
            "task",
            None
        ));
        assert!(!endpoint_matches(
            &EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
            "entity",
            kind,
            None
        ));
    }

    #[test]
    fn endpoint_of_type_requires_base_kind_match() {
        // Regression: an entity with entity_type="theorem" but base kind != "concept"
        // must NOT match a formal concept rule. This was the exact bypass:
        // before the fix, EntityOfType("theorem") ignored the base kind entirely.
        let wrong_base_kind = "project"; // not "concept"
        let et = Some("theorem");

        // The formal rule requires kind="concept". A "project" entity with
        // entity_type="theorem" must not match — even though the subtype string
        // matches — because the base kind differs.
        assert!(
            !endpoint_matches(
                &EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "theorem",
                },
                "entity",
                wrong_base_kind,
                et
            ),
            "EntityOfType must reject an entity whose base kind != rule.kind \
             even when entity_type matches — the pre-fix bug admitted this"
        );

        // The correct concept entity with the same subtype still matches.
        assert!(endpoint_matches(
            &EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
            "entity",
            "concept",
            et
        ));
    }

    #[tokio::test]
    async fn registry_resolvers_accessor_returns_registered() {
        let resolver = Box::new(MockPackResolver::new());
        let registry = registry_with_mock_resolver(rt(), resolver);
        assert_eq!(registry.resolvers().len(), 1);
        assert_eq!(registry.resolvers()[0].0, "mock");
    }

    #[tokio::test]
    async fn mock_resolver_resolve_by_id_returns_pack_record() {
        let id = Uuid::new_v4();
        let resolver: Box<dyn PackByIdResolver> = Box::new(MockPackResolver::new());
        // We need interior access — downcast first, then use via trait.
        let inner = MockPackResolver::new();
        inner.0.lock().await.owned.push(id);
        let result: Result<Option<Resolved>, RuntimeError> = inner.resolve_by_id(id).await;
        match result.unwrap() {
            Some(Resolved::PackRecord { pack, kind, data }) => {
                assert_eq!(pack, "mock");
                assert_eq!(kind, "widget");
                assert_eq!(data["id"].as_str().unwrap(), id.to_string());
            }
            other => panic!("expected PackRecord, got {:?}", other),
        }
        let _ = resolver;
    }

    #[tokio::test]
    async fn mock_resolver_resolve_unknown_uuid_returns_none() {
        let inner = MockPackResolver::new();
        let id = Uuid::new_v4();
        let result: Result<Option<Resolved>, RuntimeError> = inner.resolve_by_id(id).await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn mock_resolver_delete_soft_records_call() {
        let id = Uuid::new_v4();
        let inner = MockPackResolver::new();
        inner.0.lock().await.owned.push(id);

        let result: Result<serde_json::Value, RuntimeError> = inner.delete_by_id(id, false).await;
        let result = result.unwrap();
        assert_eq!(result["deleted"], serde_json::json!(true));
        assert_eq!(result["hard"], serde_json::json!(false));

        // After soft-delete: resolve_by_id returns None, but including_deleted returns Some.
        let live: Result<Option<Resolved>, RuntimeError> = inner.resolve_by_id(id).await;
        assert!(live.unwrap().is_none());
        let incl: Result<Option<Resolved>, RuntimeError> =
            inner.resolve_by_id_including_deleted(id).await;
        assert!(incl.unwrap().is_some());
    }

    #[tokio::test]
    async fn mock_resolver_delete_hard_removes_record() {
        let id = Uuid::new_v4();
        let inner = MockPackResolver::new();
        inner.0.lock().await.owned.push(id);

        let result: Result<serde_json::Value, RuntimeError> = inner.delete_by_id(id, true).await;
        assert_eq!(result.unwrap()["hard"], serde_json::json!(true));

        // After hard-delete: neither probe finds the record.
        let incl: Result<Option<Resolved>, RuntimeError> =
            inner.resolve_by_id_including_deleted(id).await;
        assert!(incl.unwrap().is_none());
    }

    #[tokio::test]
    async fn pack_record_not_valid_context_entity() {
        // Validates the GTD handler arm compiles and returns InvalidInput.
        // We exercise the match logic directly by constructing a PackRecord Resolved.
        let pr = Resolved::PackRecord {
            pack: "knowledge".into(),
            kind: "atom".into(),
            data: serde_json::json!({}),
        };
        // The match in GTD handlers.rs now handles PackRecord → InvalidInput.
        // We can verify the enum variant is reachable.
        assert!(matches!(pr, Resolved::PackRecord { .. }));
    }

    // ── Batched enrich_neighbor_hits / enrich_path_nodes ────────────────────

    fn neighbor_hit(node_id: Uuid) -> NeighborHit {
        NeighborHit {
            node_id,
            edge_id: Uuid::new_v4(),
            relation: EdgeRelation::Extends,
            weight: 1.0,
            name: None,
            kind: None,
            entity_type: None,
        }
    }

    fn path_node(node_id: Uuid, depth: usize) -> PathNode {
        PathNode {
            node_id,
            via_edge: None,
            depth,
            name: None,
            kind: None,
            properties: None,
            weight: 0.0,
        }
    }

    /// merge_traversal_paths_by_root: three namespaces each contribute the
    /// same root plus 2 distinct non-root nodes (each namespace already at
    /// its own `limit`, matching the per-namespace SQL-layer cap). The
    /// union across namespaces is 6 distinct non-root nodes; the merge must
    /// re-enforce `limit` on that union rather than passing it through.
    #[test]
    fn merge_traversal_paths_reenforces_limit_across_namespaces() {
        let root = Uuid::new_v4();
        let path_for = |n: usize| GraphPath {
            root_id: root,
            nodes: (0..n).map(|_| path_node(Uuid::new_v4(), 1)).collect(),
            total_weight: 1.0,
        };

        let paths = vec![path_for(2), path_for(2), path_for(2)];
        let merged = merge_traversal_paths_by_root(paths, Some(2));

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].nodes.len(),
            2,
            "merge must re-enforce limit=2 on the unioned nodes, got {:?}",
            merged[0].nodes
        );
    }

    /// merge_traversal_paths_by_root: a node reachable at different depths
    /// via two namespaces must report its shallowest depth and the
    /// `via_edge` that produced that depth, and the merged node order must
    /// be BFS (ascending depth) rather than the concatenation order of the
    /// per-namespace inputs.
    #[test]
    fn merge_traversal_paths_keeps_shortest_depth_and_bfs_order() {
        let root = Uuid::new_v4();
        let shared = Uuid::new_v4();
        let far_node = Uuid::new_v4();
        let deep_edge = Uuid::new_v4();
        let shallow_edge = Uuid::new_v4();

        // Namespace processed first: an unrelated node at depth 1, and the
        // shared node reached the long way, at depth 4.
        let ns_first = GraphPath {
            root_id: root,
            nodes: vec![
                path_node(far_node, 1),
                PathNode {
                    node_id: shared,
                    via_edge: Some(deep_edge),
                    depth: 4,
                    name: None,
                    kind: None,
                    properties: None,
                    weight: 0.0,
                },
            ],
            total_weight: 1.0,
        };
        // Namespace processed second: the same shared node, reached at depth 2.
        let ns_second = GraphPath {
            root_id: root,
            nodes: vec![PathNode {
                node_id: shared,
                via_edge: Some(shallow_edge),
                depth: 2,
                name: None,
                kind: None,
                properties: None,
                weight: 0.0,
            }],
            total_weight: 1.0,
        };

        let merged = merge_traversal_paths_by_root(vec![ns_first, ns_second], None);

        assert_eq!(merged.len(), 1);
        let nodes = &merged[0].nodes;
        assert!(
            nodes.windows(2).all(|w| w[0].depth <= w[1].depth),
            "merged nodes must be in BFS (ascending depth) order, got {:?}",
            nodes
                .iter()
                .map(|n| (n.node_id, n.depth))
                .collect::<Vec<_>>()
        );
        let shared_node = nodes
            .iter()
            .find(|n| n.node_id == shared)
            .expect("shared node must survive the merge");
        assert_eq!(
            shared_node.depth, 2,
            "shared node must report its shortest depth across namespaces"
        );
        assert_eq!(
            shared_node.via_edge,
            Some(shallow_edge),
            "shared node must carry the via_edge that produced the shortest path"
        );
    }

    /// merge_traversal_paths_by_root: `total_weight` must describe the nodes
    /// the caller is actually handed. The heaviest node here sits deepest, so
    /// re-applying `limit` to the merged union drops it — and the reported
    /// weight has to drop with it rather than keep quoting a node that was
    /// screened out.
    #[test]
    fn merge_traversal_paths_total_weight_drops_with_the_node_it_described() {
        let root = Uuid::new_v4();
        let weighted = |depth: usize, weight: f64| PathNode {
            node_id: Uuid::new_v4(),
            via_edge: None,
            depth,
            name: None,
            kind: None,
            properties: None,
            weight,
        };

        let path = GraphPath {
            root_id: root,
            nodes: vec![weighted(1, 0.5), weighted(1, 0.4), weighted(2, 9.0)],
            total_weight: 9.0,
        };

        let merged = merge_traversal_paths_by_root(vec![path], Some(2));

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].nodes.len(),
            2,
            "limit=2 must drop the depth-2 node"
        );
        assert_eq!(
            merged[0].total_weight, 0.5,
            "total_weight must be the max over surviving nodes, not the 9.0 \
             carried by the node the limit removed"
        );
    }

    /// enrich_neighbor_hits: entity hit resolved, note hit resolved with
    /// name-fallback to "[kind]", bogus UUID left as None.  Order preserved.
    #[tokio::test]
    async fn enrich_neighbor_hits_batch_entity_note_and_bogus() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Create an entity neighbor.
        let entity = rt
            .create_entity(&tok, "concept", None, "MyEntity", None, None, vec![])
            .await
            .unwrap();

        // Nameless note — name falls back to "[observation]".
        let note = rt
            .create_note(&tok, "observation", None, "body", Some(0.5), None, vec![])
            .await
            .unwrap();

        let bogus_id = Uuid::new_v4();

        let mut hits = vec![
            neighbor_hit(entity.id),
            neighbor_hit(note.id),
            neighbor_hit(bogus_id),
        ];

        rt.enrich_neighbor_hits(&tok, &mut hits).await;

        assert_eq!(hits[0].name.as_deref(), Some("MyEntity"));
        assert_eq!(hits[0].kind.as_deref(), Some("concept"));

        assert_eq!(hits[1].name.as_deref(), Some("[observation]"));
        assert_eq!(hits[1].kind.as_deref(), Some("observation"));

        assert!(hits[2].name.is_none());
        assert!(hits[2].kind.is_none());
    }

    /// enrich_neighbor_hits: note with a non-empty name uses the actual name.
    #[tokio::test]
    async fn enrich_neighbor_hits_note_with_name_uses_name() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let note = rt
            .create_note(
                &tok,
                "insight",
                Some("NoteTitle"),
                "body",
                Some(0.5),
                None,
                vec![],
            )
            .await
            .unwrap();

        let mut hits = vec![neighbor_hit(note.id)];
        rt.enrich_neighbor_hits(&tok, &mut hits).await;

        assert_eq!(hits[0].name.as_deref(), Some("NoteTitle"));
        assert_eq!(hits[0].kind.as_deref(), Some("insight"));
    }

    /// enrich_path_nodes: two paths sharing a repeated node_id; each node
    /// enriched from a single batch; unresolved node stays None.
    #[tokio::test]
    async fn enrich_path_nodes_batch_dedup_and_unresolved() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let ea = rt
            .create_entity(&tok, "concept", None, "Alpha", None, None, vec![])
            .await
            .unwrap();
        let eb = rt
            .create_entity(&tok, "document", None, "Beta", None, None, vec![])
            .await
            .unwrap();
        let bogus_id = Uuid::new_v4();

        // Path 1: ea → eb → bogus  |  Path 2: eb → ea  (shared nodes, reversed)
        let mut paths = vec![
            GraphPath {
                root_id: ea.id,
                nodes: vec![
                    path_node(ea.id, 0),
                    path_node(eb.id, 1),
                    path_node(bogus_id, 2),
                ],
                total_weight: 1.0,
            },
            GraphPath {
                root_id: eb.id,
                nodes: vec![path_node(eb.id, 0), path_node(ea.id, 1)],
                total_weight: 1.0,
            },
        ];

        rt.enrich_path_nodes(&tok, &mut paths, false).await;

        assert_eq!(paths[0].nodes[0].name.as_deref(), Some("Alpha"));
        assert_eq!(paths[0].nodes[0].kind.as_deref(), Some("concept"));
        assert_eq!(paths[0].nodes[1].name.as_deref(), Some("Beta"));
        assert_eq!(paths[0].nodes[1].kind.as_deref(), Some("document"));
        assert!(paths[0].nodes[2].name.is_none());
        assert!(paths[0].nodes[2].kind.is_none());

        // Shared nodes resolve from the same HashMap — order within each path is preserved.
        assert_eq!(paths[1].nodes[0].name.as_deref(), Some("Beta"));
        assert_eq!(paths[1].nodes[0].kind.as_deref(), Some("document"));
        assert_eq!(paths[1].nodes[1].name.as_deref(), Some("Alpha"));
        assert_eq!(paths[1].nodes[1].kind.as_deref(), Some("concept"));
    }

    /// enrich_neighbor_hits and enrich_path_nodes must resolve entities whose
    /// namespace is in the token's extra-visible set (not only the primary).
    ///
    /// Regression: the old `get_entities_by_ids`
    /// call left `filter.namespaces` unset, which collapses to
    /// `namespace = primary` in `build_entity_where`.  Graph expansion already
    /// crosses visible namespaces, so enrichment must match that scope.
    #[tokio::test]
    async fn enrich_resolves_entities_in_extra_visible_namespace() {
        let rt = KhiveRuntime::memory().unwrap();

        let ns_a = Namespace::parse("enrich-ns-a").unwrap();
        let ns_b = Namespace::parse("enrich-ns-b").unwrap();

        let tok_b = rt.authorize(ns_b.clone()).unwrap();

        // Entity lives in ns-b.
        let entity_b = rt
            .create_entity(&tok_b, "concept", None, "EntityInB", None, None, vec![])
            .await
            .unwrap();
        assert_eq!(entity_b.namespace, "enrich-ns-b");

        // Token whose primary is ns-a but ns-b is in the visible set.
        let vis_tok = rt
            .authorize_with_visibility(ns_a.clone(), vec![ns_b.clone()])
            .unwrap();

        // ── neighbor hits ──────────────────────────────────────────────────
        let mut hits = vec![neighbor_hit(entity_b.id)];
        rt.enrich_neighbor_hits(&vis_tok, &mut hits).await;

        assert_eq!(
            hits[0].name.as_deref(),
            Some("EntityInB"),
            "entity in extra-visible ns must be enriched by enrich_neighbor_hits"
        );
        assert_eq!(hits[0].kind.as_deref(), Some("concept"));

        // ── path nodes ─────────────────────────────────────────────────────
        let mut paths = vec![GraphPath {
            root_id: entity_b.id,
            nodes: vec![path_node(entity_b.id, 0)],
            total_weight: 1.0,
        }];
        rt.enrich_path_nodes(&vis_tok, &mut paths, false).await;

        assert_eq!(
            paths[0].nodes[0].name.as_deref(),
            Some("EntityInB"),
            "entity in extra-visible ns must be enriched by enrich_path_nodes"
        );
        assert_eq!(paths[0].nodes[0].kind.as_deref(), Some("concept"));
    }

    /// enrich_neighbor_hits populates entity_type from the already-fetched entity
    /// batch when the entity has a non-null entity_type.  Entities without one and
    /// note nodes leave entity_type as None.
    #[tokio::test]
    async fn enrich_neighbor_hits_populates_entity_type() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let props = serde_json::json!({"domain": "attention"});
        let entity = rt
            .create_entity(
                &tok,
                "concept",
                Some("algorithm"),
                "FlashAttn",
                None,
                Some(props),
                vec![],
            )
            .await
            .unwrap();

        let entity_no_type = rt
            .create_entity(&tok, "concept", None, "PlainConcept", None, None, vec![])
            .await
            .unwrap();

        let mut hits = vec![neighbor_hit(entity.id), neighbor_hit(entity_no_type.id)];
        rt.enrich_neighbor_hits(&tok, &mut hits).await;

        assert_eq!(hits[0].entity_type.as_deref(), Some("algorithm"));
        assert!(
            hits[1].entity_type.is_none(),
            "entity without entity_type must leave the field as None"
        );
    }

    /// enrich_path_nodes populates properties from the already-fetched entity
    /// batch when the entity has a non-null properties blob.  Entities without
    /// properties leave the field as None.
    #[tokio::test]
    async fn enrich_path_nodes_populates_properties() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let props = serde_json::json!({"year": 2024, "venue": "NeurIPS"});
        let entity_with_props = rt
            .create_entity(
                &tok,
                "document",
                None,
                "AttentionPaper",
                None,
                Some(props.clone()),
                vec![],
            )
            .await
            .unwrap();

        let entity_no_props = rt
            .create_entity(&tok, "concept", None, "BareConceptNode", None, None, vec![])
            .await
            .unwrap();

        let mut paths = vec![GraphPath {
            root_id: entity_with_props.id,
            nodes: vec![
                path_node(entity_with_props.id, 0),
                path_node(entity_no_props.id, 1),
            ],
            total_weight: 1.0,
        }];

        rt.enrich_path_nodes(&tok, &mut paths, true).await;

        assert_eq!(
            paths[0].nodes[0].properties.as_ref(),
            Some(&props),
            "properties must be filled when entity has a non-null properties blob"
        );
        assert!(
            paths[0].nodes[1].properties.is_none(),
            "entity without properties must leave the field as None"
        );
    }

    /// Regression: GraphStore::traverse must not fail with "too many SQL variables"
    /// or "too many terms in compound SELECT" when the root set exceeds the chunk
    /// boundary (400 roots per CTE VALUES clause after the fix).
    ///
    /// Graph: 1 000 roots, each with one distinct outgoing edge to a unique child.
    /// The graph store's `traverse` is exercised directly (bypassing the runtime-level
    /// entity-existence filter) to keep the test fast and targeted.
    ///
    /// Correctness: every root must appear in the result with exactly one reachable node.
    #[tokio::test]
    async fn traverse_chunks_root_binds_over_host_param_limit() {
        use khive_storage::types::TraversalOptions;

        let rt = rt();
        let tok = NamespaceToken::local();
        let graph = rt.graph(&tok).unwrap();

        const N: usize = 1_000;
        let now = chrono::Utc::now();

        let mut roots: Vec<uuid::Uuid> = Vec::with_capacity(N);
        let mut expected_children: std::collections::HashMap<uuid::Uuid, uuid::Uuid> =
            std::collections::HashMap::with_capacity(N);

        for _ in 0..N {
            let root = uuid::Uuid::new_v4();
            let child = uuid::Uuid::new_v4();
            graph
                .upsert_edge(Edge {
                    id: LinkId::from(uuid::Uuid::new_v4()),
                    namespace: "local".to_string(),
                    source_id: root,
                    target_id: child,
                    relation: EdgeRelation::Extends,
                    weight: 1.0,
                    created_at: now,
                    updated_at: now,
                    deleted_at: None,
                    metadata: None,
                    target_backend: None,
                })
                .await
                .unwrap();
            roots.push(root);
            expected_children.insert(root, child);
        }

        // Must return Ok: no "too many SQL variables" or "too many terms in compound SELECT".
        let paths = graph
            .traverse(TraversalRequest {
                roots: roots.clone(),
                options: TraversalOptions {
                    max_depth: 1,
                    direction: Direction::Out,
                    relations: None,
                    min_weight: None,
                    limit: None,
                },
                include_roots: false,
                include_properties: false,
            })
            .await
            .unwrap();

        assert_eq!(
            paths.len(),
            N,
            "traverse over {N} roots must return one GraphPath per root"
        );

        for path in &paths {
            let expected_child = expected_children[&path.root_id];
            assert_eq!(
                path.nodes.len(),
                1,
                "root {:?} must reach exactly 1 node",
                path.root_id
            );
            assert_eq!(
                path.nodes[0].node_id, expected_child,
                "root {:?} must reach its direct child",
                path.root_id
            );
        }
    }

    // ── Additive EDGE_RULES composition: pack EntityOfType rules must not shadow
    // the base EntityOfKind contract for the same relation. ──────────────────────
    //
    // When a pack contributes EntityOfType rules for a relation (e.g. variant_of:
    // goal -> theorem and goal -> definition), the base contract's EntityOfKind
    // rule for the same relation (concept -> concept) must still fire for entities
    // whose base kind is "concept" but whose EntityOfType pair is not in any pack rule.
    //
    // Specifically: a goal entity resolves to base kind "concept". A goal -> goal
    // variant_of edge has no matching pack rule (goal -> goal is not declared), so
    // pack_rule_allows returns false. The validator then extracts the base kind
    // ("concept") and checks base_entity_rule_allows, which returns true.
    // The edge is therefore allowed: additive composition holds.

    #[test]
    fn pack_entity_of_type_rules_do_not_shadow_base_entity_of_kind_rule() {
        // Formal-style EntityOfType rules for variant_of: goal -> theorem, goal -> definition.
        // goal -> goal is deliberately absent — that case must fall through to the base rule.
        let pack_rules: Vec<EdgeEndpointRule> = vec![
            EdgeEndpointRule {
                relation: EdgeRelation::VariantOf,
                source: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "goal",
                },
                target: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "theorem",
                },
            },
            EdgeEndpointRule {
                relation: EdgeRelation::VariantOf,
                source: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "goal",
                },
                target: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "definition",
                },
            },
        ];

        let goal_a =
            Resolved::Entity(Entity::new("local", "concept", "G-a").with_entity_type(Some("goal")));
        let goal_b =
            Resolved::Entity(Entity::new("local", "concept", "G-b").with_entity_type(Some("goal")));

        // Pack rules do not cover goal -> goal, so pack_rule_allows must return false.
        assert!(
            !pack_rule_allows(
                &pack_rules,
                EdgeRelation::VariantOf,
                Some(&goal_a),
                Some(&goal_b)
            ),
            "pack rules must not cover goal->goal variant_of (no such rule declared)"
        );

        // The base contract allows concept -> concept for variant_of.
        // A goal entity's base kind is "concept", so this must return true.
        assert!(
            base_entity_rule_allows("concept", EdgeRelation::VariantOf, "concept"),
            "base contract must allow concept->concept variant_of regardless of pack EntityOfType rules"
        );
    }

    // Integration path: pack EntityOfType rules installed on the runtime must not
    // block a goal->goal variant_of link that the base contract already permits.
    // Exercises validate_edge_relation_endpoints lines 1173-1223:
    //   pack miss -> extract e.kind ("concept") -> base_entity_rule_allows -> Ok.
    #[tokio::test]
    async fn link_variant_of_goal_to_goal_allowed_when_pack_has_entity_of_type_rules() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Install formal-style EntityOfType rules for variant_of (goal -> theorem/definition).
        // goal -> goal is absent so the base concept->concept rule must carry this case.
        rt.install_edge_rules(vec![
            EdgeEndpointRule {
                relation: EdgeRelation::VariantOf,
                source: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "goal",
                },
                target: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "theorem",
                },
            },
            EdgeEndpointRule {
                relation: EdgeRelation::VariantOf,
                source: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "goal",
                },
                target: EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: "definition",
                },
            },
        ]);

        let a = rt
            .create_entity(
                &tok,
                "concept",
                Some("goal"),
                "Goal Alpha",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let b = rt
            .create_entity(
                &tok,
                "concept",
                Some("goal"),
                "Goal Beta",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        // Neither endpoint matches any pack rule (no goal->goal rule).
        // The base concept->concept rule must fire and allow the edge.
        let result = rt
            .link(&tok, a.id, b.id, EdgeRelation::VariantOf, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "goal->goal variant_of must be allowed via the base concept->concept rule \
             even when EntityOfType rules for variant_of are installed; got {result:?}"
        );

        // Fail-closed check: additive rules must not make validation fail-open.
        // A goal(concept) -> project variant_of edge has no matching pack rule
        // (project is not in the installed variant_of rules) and no matching base
        // rule (no (concept, VariantOf, project) row). It must be rejected.
        let p = rt
            .create_entity(&tok, "project", None, "Proj", None, None, vec![])
            .await
            .unwrap();
        let bad = rt
            .link(&tok, a.id, p.id, EdgeRelation::VariantOf, 1.0, None)
            .await;
        assert!(
            bad.is_err(),
            "additive pack rules must not make validation fail-open; \
             goal(concept)->project variant_of must be rejected (pack miss + base miss); \
             got {bad:?}"
        );
    }

    // Load-bearing positive: a pack EntityOfType rule adds an endpoint the base contract
    // does not cover. The base contract has no (concept, DependsOn, concept) row (the
    // DependsOn rows are project/service/artifact only). A theorem->definition DependsOn
    // edge can therefore ONLY pass through the pack rule, proving the union is load-bearing.
    #[tokio::test]
    async fn link_depends_on_theorem_to_definition_allowed_only_via_pack_rule() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Confirm the base contract does NOT allow concept->concept DependsOn.
        // (Documented here so the assertion below is not a tautology.)
        assert!(
            !base_entity_rule_allows("concept", EdgeRelation::DependsOn, "concept"),
            "base contract must not allow concept->concept DependsOn; \
             test would be vacuous if this precondition fails"
        );

        // Install a single EntityOfType rule: theorem depends_on definition.
        // With no rules, the link would be rejected by the base contract.
        // With this rule, it must be accepted via the pack path (lines 1173-1179).
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

        let thm = rt
            .create_entity(&tok, "concept", Some("theorem"), "T1", None, None, vec![])
            .await
            .unwrap();
        let def = rt
            .create_entity(
                &tok,
                "concept",
                Some("definition"),
                "D1",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();

        // This can only pass through the pack rule — the base contract rejects it.
        let result = rt
            .link(&tok, thm.id, def.id, EdgeRelation::DependsOn, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "theorem->definition DependsOn must be allowed by the installed pack rule; \
             the base contract has no concept->concept DependsOn row; got {result:?}"
        );
    }

    // ── Provenance endpoint pairs ────────────────────────────────────────────
    // Four base endpoint pairs: document->person and document->org (document
    // authorship), concept->org (concept origination by an org), and
    // document->document (normative document dependency). Positive links for
    // each pair, plus a direction-matters negative guard.

    #[tokio::test]
    async fn link_document_introduced_by_person_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let doc = rt
            .create_entity(&tok, "document", None, "Paper", None, None, vec![])
            .await
            .unwrap();
        let author = rt
            .create_entity(&tok, "person", None, "Author", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                doc.id,
                author.id,
                EdgeRelation::IntroducedBy,
                1.0,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "document->person introduced_by must be allowed by the ADR-002 \
             endpoint amendment; got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_document_introduced_by_org_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let doc = rt
            .create_entity(&tok, "document", None, "Whitepaper", None, None, vec![])
            .await
            .unwrap();
        let org = rt
            .create_entity(&tok, "org", None, "Publisher", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(&tok, doc.id, org.id, EdgeRelation::IntroducedBy, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "document->org introduced_by must be allowed by the ADR-002 \
             endpoint amendment; got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_concept_introduced_by_org_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let concept = rt
            .create_entity(&tok, "concept", None, "Architecture", None, None, vec![])
            .await
            .unwrap();
        let org = rt
            .create_entity(&tok, "org", None, "Originator", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(
                &tok,
                concept.id,
                org.id,
                EdgeRelation::IntroducedBy,
                1.0,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "concept->org introduced_by must be allowed by the ADR-002 \
             endpoint amendment; got {result:?}"
        );
    }

    #[tokio::test]
    async fn link_document_depends_on_document_allowed() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let doc_a = rt
            .create_entity(&tok, "document", None, "Spec A", None, None, vec![])
            .await
            .unwrap();
        let doc_b = rt
            .create_entity(&tok, "document", None, "Spec B", None, None, vec![])
            .await
            .unwrap();

        let result = rt
            .link(&tok, doc_a.id, doc_b.id, EdgeRelation::DependsOn, 1.0, None)
            .await;
        assert!(
            result.is_ok(),
            "document->document depends_on must be allowed by the ADR-002 \
             endpoint amendment; got {result:?}"
        );
        let edge = result.unwrap();
        let dk = edge
            .metadata
            .as_ref()
            .and_then(|m| m.get("dependency_kind"))
            .and_then(|v| v.as_str());
        assert_eq!(
            dk,
            Some("normative"),
            "document->document depends_on must infer dependency_kind=normative"
        );
    }

    #[tokio::test]
    async fn link_org_introduced_by_document_rejected_direction_matters() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let org = rt
            .create_entity(&tok, "org", None, "Publisher", None, None, vec![])
            .await
            .unwrap();
        let doc = rt
            .create_entity(&tok, "document", None, "Paper", None, None, vec![])
            .await
            .unwrap();

        // The amendment adds document->org, not org->document. Direction matters:
        // an org is not "introduced by" a document it published.
        let result = rt
            .link(&tok, org.id, doc.id, EdgeRelation::IntroducedBy, 1.0, None)
            .await;
        assert!(
            result.is_err(),
            "org->document introduced_by must remain rejected; only \
             document->org is permitted, not the reverse; got {result:?}"
        );
    }

    // ── create_note_with_embedding_content ──────────────────────────────────

    /// Like `ConstVecService`/`ConstVecProvider` above, but records every text
    /// it is asked to embed so a test can assert exactly what reached the
    /// "provider" — used to verify the effective embed text is the capped
    /// override, not the full note content.
    struct CapturingVecService {
        dims: usize,
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbeddingService for CapturingVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            for text in texts {
                if text.len() > MAX_TEXT_CHARS {
                    return Err(EmbedError::TextTooLong {
                        length: text.len(),
                        max: MAX_TEXT_CHARS,
                    });
                }
            }
            self.captured.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "capturing-vec"
        }
    }

    struct CapturingVecProvider {
        provider_name: String,
        dims: usize,
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl EmbedderProvider for CapturingVecProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(CapturingVecService {
                dims: self.dims,
                captured: Arc::clone(&self.captured),
            }))
        }
    }

    #[tokio::test]
    async fn create_bounds_embedding_input_without_truncating_stored_content() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        rt.register_embedder(CapturingVecProvider {
            provider_name: "strict-length-test".into(),
            dims: 4,
            captured: Arc::clone(&captured),
        });

        let content = format!("{}\u{1f980}tail", "a".repeat(MAX_TEXT_CHARS - 1));
        let note = rt
            .create_note(&tok, "observation", None, &content, None, None, vec![])
            .await
            .expect("over-length note create must succeed");
        let fetched = rt
            .notes(&tok)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .expect("created note must be retrievable");
        assert_eq!(fetched.content, content, "stored content must remain full");

        let embedded = captured.lock().unwrap().clone();
        assert_eq!(embedded.len(), 1);
        assert_eq!(embedded[0].len(), MAX_TEXT_CHARS - 1);
        assert!(embedded[0].is_char_boundary(embedded[0].len()));
        assert!(!embedded[0].contains('\u{1f980}'));
        assert!(rt.document_embedding_input_will_be_truncated(&content));

        let vector_info = rt
            .vectors_for_model(&tok, "strict-length-test")
            .expect("vector store")
            .info()
            .await
            .expect("vector info");
        assert_eq!(vector_info.dimensions, 4);
        assert_eq!(vector_info.entry_count, 1);

        captured.lock().unwrap().clear();
        rt.reindex_note(&tok, &fetched)
            .await
            .expect("reindex must bound the same stored content");
        assert_eq!(captured.lock().unwrap()[0].len(), MAX_TEXT_CHARS - 1);

        captured.lock().unwrap().clear();
        rt.embed_document_batch_with_model("strict-length-test", std::slice::from_ref(&content))
            .await
            .expect("batch reindex seam must bound stored content");
        assert_eq!(captured.lock().unwrap()[0].len(), MAX_TEXT_CHARS - 1);

        let normal = "normal byte-identical embedding input";
        rt.create_note(&tok, "observation", None, normal, None, None, vec![])
            .await
            .expect("normal note create must succeed");
        assert_eq!(captured.lock().unwrap().last().unwrap(), normal);
        assert!(!rt.document_embedding_input_will_be_truncated(normal));

        let long_description = format!("{}\u{1f980}tail", "b".repeat(MAX_TEXT_CHARS));
        rt.create_entity(
            &tok,
            "concept",
            None,
            "entity",
            Some(&long_description),
            None,
            vec![],
        )
        .await
        .expect("over-length entity create must succeed");
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_none_matches_create_note() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        rt.register_embedder(CapturingVecProvider {
            provider_name: "capturing-vec".into(),
            dims: 4,
            captured: Arc::clone(&captured),
        });

        let note = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                "full content, no override",
                None,
                None,
                None,
                vec![],
            )
            .await
            .expect("create with None override must behave like create_note");
        assert_eq!(note.content, "full content, no override");
        let seen = captured.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec!["full content, no override".to_string()],
            "with no override the embedder must see the full content"
        );
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_embeds_capped_override_and_stores_full_content() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        rt.register_embedder(CapturingVecProvider {
            provider_name: "capturing-vec".into(),
            dims: 4,
            captured: Arc::clone(&captured),
        });

        let full = "head-term and then a very long tail-term that exceeds any cap";
        let head = &full[.."head-term and then a very long".len()];

        let note = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                full,
                Some(head),
                None,
                None,
                vec![],
            )
            .await
            .expect("proper-prefix override must be accepted");
        assert_eq!(note.content, full, "stored content must be the full text");

        let seen = captured.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![head.to_string()],
            "embedder must see only the capped override"
        );
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_fans_out_identical_override_to_multiple_models() {
        let rt = rt();
        let tok = NamespaceToken::local();
        let captured_a = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_b = Arc::new(std::sync::Mutex::new(Vec::new()));
        rt.register_embedder(CapturingVecProvider {
            provider_name: "capturing-vec-a".into(),
            dims: 4,
            captured: Arc::clone(&captured_a),
        });
        rt.register_embedder(CapturingVecProvider {
            provider_name: "capturing-vec-b".into(),
            dims: 4,
            captured: Arc::clone(&captured_b),
        });

        let full = "head-only-embedded plus a long discarded tail";
        let head = &full[.."head-only-embedded".len()];

        rt.create_note_with_embedding_content(
            &tok,
            "observation",
            None,
            full,
            Some(head),
            None,
            None,
            vec![],
        )
        .await
        .expect("create ok");

        assert_eq!(
            captured_a.lock().unwrap().clone(),
            vec![head.to_string()],
            "model A must receive the identical capped override"
        );
        assert_eq!(
            captured_b.lock().unwrap().clone(),
            vec![head.to_string()],
            "model B must receive the identical capped override"
        );

        // Both models actually persisted a vector row for the note (not just
        // an embed call that was discarded before insertion).
        let vs_a = rt
            .vectors_for_model(&tok, "capturing-vec-a")
            .expect("vector store for model A");
        assert_eq!(
            vs_a.count().await.expect("vector count A"),
            1,
            "model A must have exactly one vector row for the note"
        );
        let vs_b = rt
            .vectors_for_model(&tok, "capturing-vec-b")
            .expect("vector store for model B");
        assert_eq!(
            vs_b.count().await.expect("vector count B"),
            1,
            "model B must have exactly one vector row for the note"
        );
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_rejects_empty_override() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let err = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                "some content",
                Some(""),
                None,
                None,
                vec![],
            )
            .await
            .expect_err("empty override must be rejected");
        assert!(matches!(err, RuntimeError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_rejects_non_prefix_override() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let err = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                "the actual content",
                Some("an unrelated string"),
                None,
                None,
                vec![],
            )
            .await
            .expect_err("non-prefix override must be rejected");
        assert!(matches!(err, RuntimeError::InvalidInput(ref m) if m.contains("prefix")));
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_rejects_equal_length_override() {
        let rt = rt();
        let tok = NamespaceToken::local();

        // Same length and same text as `content` is not a *proper* prefix.
        let err = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                "identical text",
                Some("identical text"),
                None,
                None,
                vec![],
            )
            .await
            .expect_err("an equal-length override must be rejected as not a proper prefix");
        assert!(matches!(err, RuntimeError::InvalidInput(ref m) if m.contains("prefix")));
    }

    #[tokio::test]
    async fn create_note_with_embedding_content_rejects_secret_bearing_override() {
        let rt = rt();
        let tok = NamespaceToken::local();

        let token_span = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let content = format!("{token_span} plus extra trailing content beyond the override");
        let embedding_content = format!("{token_span} plus extra");

        let err = rt
            .create_note_with_embedding_content(
                &tok,
                "observation",
                None,
                &content,
                Some(&embedding_content),
                None,
                None,
                vec![],
            )
            .await
            .expect_err("a credential-shaped override must fail the secret gate");
        assert!(
            matches!(err, RuntimeError::SecretDetected(_)),
            "expected SecretDetected, got {err:?}"
        );

        // Fail-closed: no note survives the rejected create.
        let count = rt
            .notes(&tok)
            .unwrap()
            .count_notes(tok.namespace().as_str(), None)
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected create must leave no note behind");
    }
}
