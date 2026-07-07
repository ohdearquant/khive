//! ADR-099 migration step 1 (cont'd) / B3 — the per-verb async prepare pass
//! for the KG-substrate v1 admissible verbs (`update`, `delete`, `link`,
//! `merge`). Each `prepare_*` function reads current state (async, outside
//! any transaction) and returns a plain-data [`crate::atomic_runner::AtomicOpPlan`]
//! ([`crate::atomic_plan`], B1) for the synchronous commit pass ([`crate::
//! atomic_runner::run_atomic_unit`], B2) to apply.
//!
//! `gtd.transition` / `gtd.complete` prepare is deliberately **not** here:
//! their lifecycle vocabulary (`is_terminal`, `can_transition`, ...) lives in
//! `khive-pack-gtd`, a crate that depends on `khive-runtime` — not the other
//! way around. Reproducing that dependency here would invert the crate
//! graph, so their prepare functions live in `kkernel` (which already
//! depends on both `khive-runtime` and `khive-pack-gtd`), calling back into
//! the plain [`PlanStatement`]/[`AffectedRowGuard`] shapes exported from this
//! module's sibling, [`crate::atomic_plan`].
//!
//! `propose` / `review` / `withdraw` (the ADR-046 event-sourced governance
//! lifecycle) are on the v1 admissible list ([`khive_types::pack::
//! ATOMIC_ADMISSIBLE_VERBS`]) but have no prepare implementation here: their
//! apply path is a changeset-interpreter (`apply_worker`) over a dedicated
//! `proposals_open` table, not a small number of guarded DML statements — a
//! faithful, non-stub atomic prepare for them is separate follow-on work.
//! [`prepare_governance_unimplemented`] fails loudly, before any write,
//! naming this as a known scope gap rather than silently no-opping.
//!
//! `merge` is likewise on the v1 admissible list but is deferred (B3 fix
//! round, Leo refinement 2026-07-07): full-parity field folding, survivor
//! index reindex, loser index purge, provenance, and same-kind rejection are
//! achievable as static DML, but `curation::merge_entity_sql`'s graceful
//! edge-conflict resolution is not (it is per-row procedural, incompatible
//! with ADR-099 D1's static predicate/guard plan shape) — rather than ship a
//! partially-scoped atomic merge, it is rejected at the same pre-runtime
//! static guard as governance
//! ([`khive_types::pack::ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`]). `prepare_merge`
//! below is therefore unreachable through `--atomic`; it remains only as the
//! pre-fix-round direct-prepare implementation, exercised by this module's
//! own tests, and as defense in depth.

use serde_json::Value;
use uuid::Uuid;

use khive_storage::types::SqlValue;
use khive_storage::{EdgeRelation, SqlStatement};
use khive_types::{EventKind, SubstrateKind};

use crate::atomic_plan::{
    AffectedRowGuard, DeletePlan, EdgeNaturalKey, LinkPlan, MergePlan, PlanStatement,
    PostCommitEffect, UpdatePlan,
};
use crate::atomic_runner::AtomicOpPlan;
use crate::error::{RuntimeError, RuntimeResult};
use crate::operations::{
    canonical_edge_endpoints, merge_dependency_kind, validate_edge_metadata, validate_edge_weight,
    Resolved,
};
use crate::runtime::{KhiveRuntime, NamespaceToken};

use khive_db::stores::entity::{
    entity_hard_delete_statement, entity_soft_delete_statement, entity_upsert_statement,
};
use khive_db::stores::event::event_insert_statements;
use khive_db::stores::graph::{
    edge_hard_delete_statement, edge_insert_guarded_by_endpoints_statement,
    edge_soft_delete_statement, edge_symmetric_delete_if_conflict_statement,
    edge_symmetric_refresh_or_update_inplace_statement, edge_upsert_statement,
    purge_incident_edges_statement,
};
use khive_db::stores::note::{
    note_hard_delete_statement, note_soft_delete_statement, note_upsert_statement,
};

// ---------------------------------------------------------------------------
// arg extraction helpers
// ---------------------------------------------------------------------------

fn obj(args: &Value) -> RuntimeResult<&serde_json::Map<String, Value>> {
    args.as_object()
        .ok_or_else(|| RuntimeError::InvalidInput("op args must be a JSON object".into()))
}

fn require_str<'a>(args: &'a Value, key: &str) -> RuntimeResult<&'a str> {
    obj(args)?
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing required field {key:?}")))
}

fn require_uuid(args: &Value, key: &str) -> RuntimeResult<Uuid> {
    let raw = require_str(args, key)?;
    Uuid::parse_str(raw)
        .map_err(|_| RuntimeError::InvalidInput(format!("{key} must be a full UUID; got {raw:?}")))
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    obj(args).ok()?.get(key).and_then(|v| v.as_str())
}

/// Nullable-string patch semantics (ADR-099 B3 fix round 5, finding 2 —
/// codex r3 REJECT). Mirrors the *actually reachable* behavior of
/// `khive-pack-kg::handlers::common::optional_string_patch`/
/// `description_patch`, reimplemented here rather than imported (that
/// module is `pub(crate)` to a sibling crate with no dependency edge back to
/// `khive-runtime`). Canonical's field type is `Option<Value>`
/// (`UpdateParams.name`/`.description`); serde_json's derived
/// `Deserialize` for `Option<T>` intercepts a literal JSON `null` at the
/// OUTER Option boundary and maps it straight to Rust `None` — REGARDLESS
/// of the inner type `T` — so canonical's own `Some(Value::Null) =>
/// Ok(Some(None))` "clear" arm is unreachable through normal struct
/// deserialization (empirically verified against the live `handle_update`:
/// `update(name=null)` / `update(description=null)` are no-ops, not
/// clears). This module reads raw, un-deserialized JSON, so it must
/// replicate that collapse explicitly: key absent OR JSON `null` -> `None`
/// (leave unchanged, no-op); key present as a string -> `Some(Some(s))`
/// (set); any other JSON type -> a hard error (canonical's still-reachable
/// non-null-non-string rejection).
fn optional_string_patch(args: &Value, key: &str) -> RuntimeResult<Option<Option<String>>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// Strict string-or-absent-or-null patch for entity `name` (ADR-099 B3 fix
/// round 5, finding 1 of codex r3's High finding 2 — the actual violation:
/// `optional_str`'s `.as_str()` silently drops a non-string, non-null value
/// like `name: 123` as absent, reporting success for an invalid update.
/// Canonical validates entity `name` via `string_value` (common.rs:819) on
/// `UpdateParams.name: Option<Value>` — null collapses to absent at the
/// struct-deserialize boundary (see `optional_string_patch` doc above), so
/// `string_value`'s reachable behavior is: absent/null -> unchanged;
/// non-null string -> set; any other JSON type -> hard error. This mirrors
/// exactly that, reading raw JSON instead of a deserialized struct.
fn entity_name_patch(args: &Value) -> RuntimeResult<Option<String>> {
    match obj(args)?.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "name must be a string, got: {other}"
        ))),
    }
}

/// Nullable-JSON-value patch for `properties` (ADR-099 B3 fix round 5,
/// finding 2): canonical `properties: Option<Value>` on `UpdateParams`
/// collapses a literal JSON `null` to Rust `None` at the struct-deserialize
/// boundary (same gotcha as `optional_string_patch` above), so
/// `properties=null` is canonically a no-op (leave existing properties
/// unchanged) — NOT a stored JSON `null`. This module reads raw JSON, so it
/// must replicate that collapse: key absent OR JSON `null` -> `None` (no
/// merge); any other JSON value -> `Some(value)` (merge), matching
/// canonical's un-typed pass-through (no further shape validation at this
/// layer either way).
fn optional_properties(args: &Value, key: &str) -> RuntimeResult<Option<Value>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(v.clone())),
    }
}

/// `tags` patch (ADR-099 B3 fix round 5, finding 2): canonical
/// `tags: Option<Vec<String>>` on `UpdateParams` collapses a literal JSON
/// `null` to Rust `None` at the struct-deserialize boundary (same gotcha),
/// so `tags=null` is canonically a no-op (leave existing tags unchanged) —
/// the pre-fix version of this function instead ERRORED on null, which is
/// the exact divergence codex flagged. A non-array, non-null value is still
/// a hard error (mirrors the type failure `UpdateParams` deserialization
/// would itself produce for a malformed `tags`).
fn optional_tags(args: &Value) -> RuntimeResult<Option<Vec<String>>> {
    match obj(args)?.get("tags") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                let s = item.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput("tags must be an array of strings".into())
                })?;
                tags.push(s.to_string());
            }
            Ok(Some(tags))
        }
        Some(_) => Err(RuntimeError::InvalidInput(
            "tags must be an array of strings".into(),
        )),
    }
}

fn optional_f64(args: &Value, key: &str) -> RuntimeResult<Option<f64>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a number"))),
    }
}

/// Tri-state patch extraction for `Option<Option<f64>>`-shaped fields
/// (`NotePatch::salience` / `NotePatch::decay_factor`): key absent -> `None`
/// (untouched), key present as JSON `null` -> `Some(None)` (clear), key
/// present as a number -> `Some(Some(v))` (set). Preserves the atomic path's
/// pre-existing `contains_key`-based clear/set/untouched semantics now that
/// range validation itself has moved into curation.rs's `prepare_update_note`.
fn optional_f64_patch(args: &Value, key: &str) -> RuntimeResult<Option<Option<f64>>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(v) => v
            .as_f64()
            .map(|f| Some(Some(f)))
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a number"))),
    }
}

/// Every registered embedding model's vector table name, in the exact format
/// `curation::merge_entity_sql` uses (`"vec_{sanitize_key(model_name)}"`) —
/// reused here so atomic delete/merge purge the same tables the non-atomic
/// paths do.
fn vector_table_names(runtime: &KhiveRuntime) -> Vec<String> {
    runtime
        .registered_embedding_model_names()
        .iter()
        .map(|name| format!("vec_{}", crate::config::sanitize_key(name)))
        .collect()
}

/// A guarded (`guard: None` — best-effort mirror, matching the non-atomic
/// index-cleanup calls which don't assert a row existed) `DELETE` statement
/// against one FTS or vector table for a single subject, scoped by namespace.
fn purge_index_row_statement(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    label: &str,
) -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: format!("DELETE FROM {table} WHERE namespace = ?1 AND subject_id = ?2"),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(subject_id.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

/// `true` iff a table named `table` currently exists in the backing SQLite
/// database (`sqlite_master` probe, read-only — safe in async prepare, does
/// NOT open/create the vector store, so it cannot lazily create the table
/// itself).
async fn vector_table_exists(runtime: &KhiveRuntime, table: &str) -> RuntimeResult<bool> {
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let row = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1".to_string(),
            params: vec![SqlValue::Text(table.to_string())],
            label: Some("atomic-delete-vec-table-exists".to_string()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(row.is_some())
}

/// Append the FTS + every registered model's vector-row purge for `subject_id`
/// (scoped to the RECORD's own namespace, matching `delete_entity`/
/// `delete_note`'s `record_tok`/`record_ns` convention — NOT the caller
/// token's namespace, ADR-007 rule 2 by-ID namespace-agnosticism) onto
/// `statements`.
///
/// FTS tables (`fts_entities`/`fts_notes`) always exist (created at schema
/// migration time) so their purge is unconditional. `vec_*` tables are
/// created LAZILY on first vector-store open (`vectors_for_model` ->
/// `CREATE VIRTUAL TABLE IF NOT EXISTS`, khive-db backend.rs) — a default
/// runtime registers embedding models before any vector table necessarily
/// exists, so a raw unconditional `DELETE FROM vec_*` can hit `no such
/// table` on a fresh DB (codex r2 Blocker 1). Only push the vec purge for
/// tables that actually exist: absence means the record definitionally has
/// no vector row for that model, so skipping is data-parity-correct (the
/// non-atomic path would lazily create the table then delete zero rows —
/// same DATA outcome, the only difference is an init side-effect this
/// read-only prepare pass must not perform).
async fn push_index_purge_statements(
    runtime: &KhiveRuntime,
    statements: &mut Vec<PlanStatement>,
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
) -> RuntimeResult<()> {
    statements.push(purge_index_row_statement(
        fts_table,
        namespace,
        subject_id,
        &format!("{label_prefix}-purge-fts"),
    ));
    for vec_table in vector_table_names(runtime) {
        if vector_table_exists(runtime, &vec_table).await? {
            statements.push(purge_index_row_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-purge-vec-{vec_table}"),
            ));
        }
    }
    Ok(())
}

/// Event-store append parity for the canonical handlers that emit a
/// lifecycle event AFTER their row mutation: `update_entity` -> `EntityUpdated`
/// (curation.rs:257-273), `delete_entity` -> `EntityDeleted`
/// (operations.rs:3543-3558), `delete_note` -> `NoteDeleted`
/// (operations.rs:3326-3340), `update_edge` -> `EdgeUpdated`
/// (operations.rs:3968-3983), `delete_edge` -> `EdgeDeleted`
/// (operations.rs:4042-4056). `update_note` and `link` append no event
/// (parity boundary verified by the GAP-1 sweep) and must never call this.
///
/// ADR-099 B3 r6 (Leo condition 2, "one event implementation"): builds the
/// `Event` exactly as each canonical site does
/// (`khive_storage::event::Event::new(...).with_target(...).with_payload(...)`)
/// and turns it into plain-data `SqlStatement`s via
/// [`khive_db::stores::event::event_insert_statements`] — the SAME builder
/// [`khive_db::stores::event::append_event_on_writer`] (the async execution
/// path every canonical `event_store.append_event(...)` call ultimately
/// reaches) uses. There is exactly one place that knows the
/// `events`/`event_observations` insert shape; this function only adapts its
/// output into unguarded [`PlanStatement`]s for the atomic-unit plan.
///
/// Placement decision (matches canonical's error semantics, ADR-099 B3 GAP-1
/// fix round): all canonical sites call
/// `event_store.append_event(event).await.map_err(...)?` — the `?` makes a
/// failed append a hard `RuntimeError`, never swallowed/logged-and-continue.
/// The insert is two-to-N plain, deterministic `INSERT`s (`events`, then
/// `event_observations` for the target-row observation) computed entirely
/// from data already on hand at prepare time — no embedding call, no other
/// suspending/async computation, unlike the `ReindexEntity`/`ReindexNote`
/// post-commit effects this same module defers for exactly that reason. A
/// fatal, purely-SQL-expressible effect is the `PlanStatement`-inside-the-
/// atomic-unit case (not `PostCommitEffect`, which this module reserves for
/// best-effort or non-SQL work): committing the event row atomically with
/// the mutation it describes only STRENGTHENS canonical's guarantee (the
/// non-atomic handlers write the event in a *separate* transaction, ordered
/// but not atomic with the row mutation).
///
/// Returned statements are unguarded — appended after the plan's own guarded
/// row statement, so [`apply_plan`]'s stop-on-first-failure contract means
/// they are only reached once that row mutation's guard has already held
/// (mirroring canonical's `if deleted { append_event(...) }` /
/// unconditional-after-`upsert_entity` shape).
fn event_append_statements(
    namespace: &str,
    verb: &str,
    kind: EventKind,
    substrate: SubstrateKind,
    target_id: Uuid,
    payload: Value,
) -> RuntimeResult<Vec<PlanStatement>> {
    let event = khive_storage::event::Event::new(namespace.to_string(), verb, kind, substrate, "")
        .with_target(target_id)
        .with_payload(payload);
    let statements = event_insert_statements(&event)
        .map_err(|e| RuntimeError::Internal(format!("event_insert_statements: {e}")))?;
    Ok(statements
        .into_iter()
        .map(|statement| PlanStatement {
            statement,
            guard: None,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Build the prepared [`AtomicOpPlan`] for one KG-substrate admissible op
/// (`update`, `delete`, `link`, `merge`). Returns a loud [`RuntimeError`] for
/// `propose`/`review`/`withdraw` (known scope gap, see module doc) and any
/// other verb (the CLI boundary must reject those before calling this — a
/// verb reaching here is either KG-substrate-admissible or a bug upstream).
pub async fn prepare_op(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tool: &str,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    match tool {
        // `expected_kind: None` here — same reasoning as the `"delete"` arm
        // immediately below: callers that need `update(kind=...)` parity
        // (ADR-099 B3 r7, codex r7 Blocker finding 1) must resolve the kind
        // spec themselves (it needs a `VerbRegistry`, unreachable from this
        // crate — see `AtomicUpdateKind`'s doc comment) and call
        // `prepare_update` directly with the resolved value; `kkernel`'s
        // `--atomic` seam does exactly this and bypasses this dispatch arm
        // for "update". A caller reaching `prepare_op("update", ...)`
        // without going through that seam gets kind-unchecked behavior,
        // same as delete's pre-existing arm.
        "update" => prepare_update(runtime, token, args, None).await,
        // `expected_kind: None` here — callers that need `delete(kind=...)`
        // parity (ADR-099 B3 fix round 5, finding 1) must resolve the kind
        // spec themselves (it needs a `VerbRegistry`, unreachable from this
        // crate — see `AtomicDeleteKind`'s doc comment) and call
        // `prepare_delete` directly with the resolved value; `kkernel`'s
        // `--atomic` seam does exactly this and bypasses this dispatch arm
        // for "delete". A caller reaching `prepare_op("delete", ...)`
        // without going through that seam gets kind-unchecked behavior,
        // same as before this fix round.
        "delete" => prepare_delete(runtime, token, args, None).await,
        "link" => prepare_link(runtime, token, args).await,
        "merge" => prepare_merge(runtime, token, args).await,
        "propose" | "review" | "withdraw" => prepare_governance_unimplemented(tool),
        other => Err(RuntimeError::InvalidInput(format!(
            "{other:?} has no atomic_prepare::prepare_op implementation; the CLI \
             admissibility check should have rejected this before prepare"
        ))),
    }
}

fn prepare_governance_unimplemented(tool: &str) -> RuntimeResult<AtomicOpPlan> {
    Err(RuntimeError::InvalidInput(format!(
        "{tool:?} is on the ADR-099 v1 admissible verb list but has no --atomic \
         prepare/apply implementation yet: its lifecycle (ADR-046) is an \
         event-sourced changeset-interpreter over a dedicated `proposals_open` \
         table, not a small guarded-DML plan — a faithful non-stub atomic \
         prepare for it is tracked as ADR-099 follow-up work, not implemented \
         in slice B3. No write was attempted."
    )))
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// Mirrors `khive-pack-kg::handlers::update::reject_inapplicable_fields`
/// (GAP-4, B3 fix round 4): a hard `InvalidInput` when a caller passes a
/// field that does not apply to the resolved substrate (e.g. `salience` on
/// an entity, or `description`/`tags` on a note). That function is
/// `pub(crate)` to a sibling crate with no dependency edge back to
/// `khive-runtime` (`khive-pack-kg` depends on `khive-runtime`, not the
/// other way around), so its exact field-applicability check list and error
/// message shape are reimplemented here rather than imported — same pattern
/// as `optional_string_patch` above. Presence is checked directly on the raw
/// args object (this module has no `UpdateParams` struct); a JSON `null`
/// value is treated as absent, matching `Option<T>` deserialization
/// semantics for the fields update.rs's checklist covers.
fn reject_inapplicable_update_fields(args: &Value, substrate: &str) -> RuntimeResult<()> {
    let o = obj(args)?;
    let present = |k: &str| o.get(k).is_some_and(|v| !v.is_null());
    let (bad_field, valid): (Option<&str>, &str) = match substrate {
        "entity" => {
            let bad = if present("content") {
                Some("content")
            } else if present("salience") {
                Some("salience")
            } else if present("decay_factor") {
                Some("decay_factor")
            } else if present("relation") {
                Some("relation")
            } else if present("weight") {
                Some("weight")
            } else {
                None
            };
            (bad, "name, description, tags, properties")
        }
        "note" => {
            let bad = if present("description") {
                Some("description")
            } else if present("tags") {
                Some("tags")
            } else if present("relation") {
                Some("relation")
            } else if present("weight") {
                Some("weight")
            } else {
                None
            };
            (bad, "name, content, salience, decay_factor, properties")
        }
        // ADR-099 B3 r6: closes the round-4 codex REJECT (High) — `update`
        // admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS` but this
        // validator previously had no "edge" arm at all, so an edge update
        // carrying an entity/note-only field (e.g. `name`) silently skipped
        // the guard instead of being rejected, mirroring
        // `khive-pack-kg::handlers::update::reject_inapplicable_fields`'s
        // `KindSpec::Edge` arm.
        "edge" => {
            let bad = if present("name") {
                Some("name")
            } else if present("description") {
                Some("description")
            } else if present("content") {
                Some("content")
            } else if present("tags") {
                Some("tags")
            } else if present("salience") {
                Some("salience")
            } else if present("decay_factor") {
                Some("decay_factor")
            } else {
                None
            };
            (bad, "relation, weight, properties")
        }
        _ => (None, ""),
    };
    if let Some(field) = bad_field {
        let substrate_label = match substrate {
            "entity" => "an entity",
            "note" => "a note",
            "edge" => "an edge",
            other => other,
        };
        return Err(RuntimeError::InvalidInput(format!(
            "field '{field}' is not valid for {substrate_label}; valid fields: {valid}"
        )));
    }
    Ok(())
}

/// Caller-supplied update-kind expectation, resolved via the canonical
/// `resolve_kind_spec` at the kkernel `--atomic` seam — the exact same
/// pattern [`AtomicDeleteKind`] uses (ADR-099 B3 r7, codex r7 Blocker
/// finding 1: `update(kind="document", id=<concept>)` was canonically
/// `NotFound` but the atomic path ignored the explicit kind and mutated the
/// resolved entity anyway). `khive-runtime` must not depend on
/// `khive-pack-kg`, so this is a plain substrate-level shape rather than
/// `khive_pack_kg::handlers::KindSpec` itself — the kkernel seam does the
/// pack-aware resolution and passes down only what `prepare_update` needs
/// to enforce the mismatch check.
pub enum AtomicUpdateKind {
    Entity { specific: Option<String> },
    Note { specific: Option<String> },
    Edge,
}

/// `expected_kind`: `None` when the caller omitted `kind` (no check, parity
/// with canonical's own optional discriminator); `Some(_)` enforces an
/// exact-parity mismatch check against the resolved record's actual
/// substrate/specific kind, mirroring `handle_update`'s
/// `entity.kind != *k` / note kind checks (update.rs:200-201, :229-234).
pub async fn prepare_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    expected_kind: Option<AtomicUpdateKind>,
) -> RuntimeResult<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;

    // GAP-4 (B3 fix round 4): mirrors update.rs's entity_kind immutability
    // guard (update.rs:160-164), which the pre-fix atomic prepare never
    // checked at all — entity_kind is a legacy top-level field, independent
    // of the `kind` substrate discriminator handled elsewhere.
    if obj(args)?.get("entity_kind").is_some_and(|v| !v.is_null()) {
        return Err(RuntimeError::InvalidInput(
            "entity_kind is immutable; to change kind, delete then re-create the entity, \
             or use merge() if this is a deduplication correction"
                .into(),
        ));
    }

    match runtime.resolve_by_id(token, id).await? {
        Some(Resolved::Entity(entity)) => {
            match &expected_kind {
                None => {}
                Some(AtomicUpdateKind::Entity {
                    specific: Some(expected),
                }) => {
                    if &entity.kind != expected {
                        return Err(RuntimeError::NotFound(format!("entity {id}")));
                    }
                }
                Some(AtomicUpdateKind::Entity { specific: None }) => {}
                Some(AtomicUpdateKind::Note { .. }) => {
                    return Err(RuntimeError::NotFound(format!("note {id}")));
                }
                Some(AtomicUpdateKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            // Decide step lives in curation.rs's `prepare_update_entity` —
            // the SAME function canonical `update_entity` calls. Only the
            // arg-extraction (raw JSON -> `EntityPatch`) and the plan-shape
            // wiring (domain object -> `PlanStatement` via the shared
            // `entity_upsert_statement` builder) are atomic-path-specific.
            reject_inapplicable_update_fields(args, "entity")?;
            let name = entity_name_patch(args)?;
            let description = optional_string_patch(args, "description")?;
            let properties = optional_properties(args, "properties")?;
            let tags = optional_tags(args)?;

            let (entity, text_changed, changed_fields) = runtime
                .prepare_update_entity(
                    token,
                    id,
                    crate::curation::EntityPatch {
                        name,
                        description,
                        properties,
                        tags,
                    },
                )
                .await?;

            let mut statements = vec![PlanStatement {
                statement: entity_upsert_statement(&entity),
                guard: Some(AffectedRowGuard::exactly(1)),
            }];
            // GAP-1 (B3 fix round): curation.rs's `update_entity` appends an
            // `EntityUpdated` event unconditionally after `upsert_entity`
            // succeeds, regardless of `text_changed` — match that here, not
            // just on the reindex-triggering subset of updates.
            statements.extend(event_append_statements(
                &entity.namespace,
                "update",
                EventKind::EntityUpdated,
                SubstrateKind::Entity,
                id,
                serde_json::json!({
                    "id": id,
                    "namespace": entity.namespace,
                    "changed_fields": changed_fields,
                }),
            )?);
            let post_commit = if text_changed {
                PostCommitEffect::ReindexEntity { entity_id: id }
            } else {
                PostCommitEffect::None
            };
            Ok(AtomicOpPlan::Update(UpdatePlan {
                target_id: id,
                statements,
                post_commit,
                edge_natural_key: None,
            }))
        }
        Some(Resolved::Note(note)) => {
            match &expected_kind {
                None => {}
                Some(AtomicUpdateKind::Note {
                    specific: Some(expected),
                }) => {
                    if &note.kind != expected {
                        return Err(RuntimeError::NotFound(format!("note {id}")));
                    }
                }
                Some(AtomicUpdateKind::Note { specific: None }) => {}
                Some(AtomicUpdateKind::Entity { .. }) => {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
                Some(AtomicUpdateKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            // Decide step lives in curation.rs's `prepare_update_note` — the
            // SAME function canonical `update_note` calls, including the
            // salience/decay_factor range validation. `optional_f64_patch`
            // below preserves the pre-existing atomic tri-state semantics
            // (key absent = untouched, key null = clear, key present = set)
            // when constructing the `NotePatch`.
            reject_inapplicable_update_fields(args, "note")?;
            let name = optional_string_patch(args, "name")?;
            let content = optional_str(args, "content").map(|s| s.to_string());
            let properties = optional_properties(args, "properties")?;
            let salience = optional_f64_patch(args, "salience")?;
            let decay_factor = optional_f64_patch(args, "decay_factor")?;

            let (note, text_changed) = runtime
                .prepare_update_note(
                    token,
                    id,
                    crate::curation::NotePatch::new(
                        name,
                        content,
                        salience,
                        decay_factor,
                        properties,
                    ),
                )
                .await?;

            let post_commit = if text_changed {
                PostCommitEffect::ReindexNote { note_id: id }
            } else {
                PostCommitEffect::None
            };
            Ok(AtomicOpPlan::Update(UpdatePlan {
                target_id: id,
                statements: vec![PlanStatement {
                    statement: note_upsert_statement(&note),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                post_commit,
                edge_natural_key: None,
            }))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "update target {id} must be an entity, note, or edge"
        ))),
        // `Resolved` (khive-runtime::operations) has no `Edge` variant — an
        // id that isn't an entity/note/pack-private/event record is checked
        // against the graph store directly, mirroring
        // `khive-pack-kg::handlers::KgPack::infer_kind_from_uuid`'s own
        // entity/note-then-edge fallback order (ADR-099 B3 r6, closes the
        // round-4 codex REJECT: `update` admits `kind="edge"` but this
        // function previously had no path that could ever build a plan for
        // one).
        None => match &expected_kind {
            Some(AtomicUpdateKind::Entity { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicUpdateKind::Note { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicUpdateKind::Edge) | None => match runtime.get_edge(token, id).await? {
                Some(edge) => prepare_update_edge(runtime, id, edge, args).await,
                None => Err(RuntimeError::NotFound(format!("entity/note/edge {id}"))),
            },
        },
    }
}

/// Edge branch of `prepare_update` (ADR-099 B3 r6). Mirrors
/// `khive-runtime::operations::KhiveRuntime::update_edge`'s patch semantics
/// exactly: `relation`/`weight`/`properties` are the only applicable fields
/// (`reject_inapplicable_update_fields`'s `"edge"` arm enforces this before
/// any mutation), a changed `relation` is endpoint-validated first, `weight`
/// is range-checked, and `properties` REPLACES `metadata` wholesale (no
/// merge — `update_edge` does `edge.metadata = Some(props)`, unlike the
/// entity/note branches' `merge_properties`).
///
/// DML shape:
/// - non-symmetric relation: a single [`edge_upsert_statement`] call on the
///   patched `Edge` — bit-for-bit the same builder `update_edge`'s own
///   non-symmetric branch calls via `graph.upsert_edge(edge.clone())`
///   (`khive-db::stores::graph::SqlGraphStore::upsert_edge`), so parity is
///   exact by construction.
/// - symmetric relation (`competes_with`, `composed_with`): `update_edge`
///   does NOT use the upsert builder here — its comment explains why
///   (`upsert_edge` resolves `ON CONFLICT(namespace, id)` first and cannot
///   detect a natural-key collision with a *different* id). Canonical
///   (`update_edge_symmetric_dml`) runs a conflict probe and branches in
///   Rust inside a single uninterrupted transaction, which is safe there.
///   This atomic path (ADR-099 B3 r7, codex r7 High finding 3) does NOT
///   branch on a prepare-time probe: the prepare/commit phase split means a
///   different op in the SAME atomic unit could change the conflict
///   landscape between this probe and commit, so a Rust-level branch here
///   would be stale by construction. Instead it always emits BOTH
///   statements from [`edge_symmetric_delete_if_conflict_statement`] and
///   [`edge_symmetric_refresh_or_update_inplace_statement`] — each carries
///   its own commit-time `WHERE`/`CASE WHEN` predicate that re-evaluates the
///   conflict condition fresh inside the transaction, so the write is
///   correct regardless of prepare-time state. ADR-099 B3 r9 (codex r8
///   Blocker finding 1) removed the prepare-time conflict probe entirely —
///   this function no longer reads any state to guess a surviving id; the
///   plan instead carries `edge_natural_key` (the plain canonicalized
///   endpoints/relation this update targets), letting a post-commit caller
///   derive the actual surviving id from the committed row, never from a
///   value computed before the rest of this atomic unit has even run.
async fn prepare_update_edge(
    runtime: &KhiveRuntime,
    id: Uuid,
    mut edge: khive_storage::types::Edge,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    reject_inapplicable_update_fields(args, "edge")?;

    let relation_raw = optional_str(args, "relation");
    let weight = optional_f64(args, "weight")?;
    let properties = optional_properties(args, "properties")?;

    if let Some(ref p) = properties {
        crate::secret_gate::check_json(p)?;
    }

    let namespace = edge.namespace.clone();
    let record_tok = NamespaceToken::for_namespace(
        khive_types::Namespace::parse(&namespace)
            .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
    );

    let mut changed_fields: Vec<&'static str> = Vec::new();
    if let Some(raw) = relation_raw {
        let relation = parse_edge_relation(raw)?;
        runtime
            .validate_edge_relation_endpoints(&record_tok, edge.source_id, edge.target_id, relation)
            .await?;
        edge.relation = relation;
        changed_fields.push("relation");
    }
    if let Some(w) = weight {
        if !w.is_finite() || !(0.0..=1.0).contains(&w) {
            return Err(RuntimeError::InvalidInput(format!(
                "edge weight must be a finite value in [0.0, 1.0]; got {w}"
            )));
        }
        edge.weight = w;
        changed_fields.push("weight");
    }
    if let Some(p) = properties {
        edge.metadata = Some(p);
        changed_fields.push("properties");
    }

    let (canon_src, canon_tgt) =
        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
    let now = chrono::Utc::now();

    let mut statements: Vec<PlanStatement> = Vec::new();
    let mut edge_natural_key: Option<EdgeNaturalKey> = None;

    if edge.relation.is_symmetric() {
        // ADR-099 B3 r7 (codex r7 High finding 3): the WRITE for a symmetric
        // relation no longer branches on a prepare-time probe result — it
        // ALWAYS carries both self-guarding, commit-time-predicate
        // statements (see their doc comment in khive-db's graph.rs for the
        // full rationale). This closes the staleness window a prepare-time
        // probe exposed atomic to (an earlier op in the SAME atomic unit
        // could change the conflict landscape before commit) without
        // touching canonical's own probe-then-branch `update_edge_symmetric_dml`
        // (which has no such exposure — single transaction, no interleaving
        // — and stays as the control-group, tests untouched).
        let metadata_str = edge
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        statements.push(PlanStatement {
            statement: edge_symmetric_delete_if_conflict_statement(
                &namespace,
                id,
                canon_src,
                canon_tgt,
                edge.relation,
            ),
            guard: Some(AffectedRowGuard {
                expected_min: 0,
                expected_max: Some(1),
            }),
        });
        statements.push(PlanStatement {
            statement: edge_symmetric_refresh_or_update_inplace_statement(
                &namespace,
                id,
                canon_src,
                canon_tgt,
                edge.relation,
                edge.weight,
                now.timestamp_micros(),
                metadata_str.as_deref(),
                edge.target_backend.as_deref(),
            ),
            guard: Some(AffectedRowGuard::exactly(1)),
        });

        // No prepare-time read needed: the two statements above are
        // self-guarding at commit time (see their doc comment). Post-commit
        // result rendering derives the actual surviving id from THIS
        // natural key, never from a value computed here.
        edge_natural_key = Some(EdgeNaturalKey {
            namespace: namespace.clone(),
            canon_source_id: canon_src,
            canon_target_id: canon_tgt,
            relation: edge.relation,
        });
    } else {
        // Non-symmetric: bit-for-bit the same builder `graph.upsert_edge`
        // calls — see doc comment above.
        edge.updated_at = now;
        statements.push(PlanStatement {
            statement: edge_upsert_statement(&edge),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }

    // Mirrors `update_edge`'s unconditional post-mutation `EdgeUpdated`
    // event append (operations.rs:3968-3983), keyed on the ORIGINAL
    // `edge_id` the caller supplied — canonical does the same (the event
    // target is `edge_id`, not the post-absorption surviving id).
    statements.extend(event_append_statements(
        &namespace,
        "update",
        EventKind::EdgeUpdated,
        SubstrateKind::Entity,
        id,
        serde_json::json!({"id": id, "namespace": namespace, "changed_fields": changed_fields}),
    )?);

    Ok(AtomicOpPlan::Update(UpdatePlan {
        target_id: id,
        statements,
        post_commit: PostCommitEffect::None,
        edge_natural_key,
    }))
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

/// Caller-supplied delete-kind expectation, resolved via the canonical
/// `resolve_kind_spec` at the kkernel `--atomic` seam (ADR-099 B3 fix round
/// 5, finding 1 — codex r3 REJECT Blocker). `khive-runtime` must not depend
/// on `khive-pack-kg` (packs depend on the runtime, not the other way
/// around), so this is a plain substrate-level shape rather than
/// `khive_pack_kg::handlers::KindSpec` itself: the kkernel seam does the
/// pack-aware `resolve_kind_spec` resolution (which needs a `VerbRegistry`,
/// unreachable from this crate) and passes down only what `prepare_delete`
/// needs to enforce the mismatch check.
///
/// `Edge` was added in ADR-099 B3 r6, closing the round-4 codex REJECT
/// (High): `delete` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`, but
/// this enum previously had no variant for it, so the kkernel seam rejected
/// `delete(kind="edge", ...)` before `prepare_delete` was ever reached —
/// admissible-but-unimplemented. `Event`/`Proposal` remain rejected at the
/// kkernel seam (not v1-admissible for atomic delete at all).
pub enum AtomicDeleteKind {
    Entity { specific: Option<String> },
    Note { specific: Option<String> },
    Edge,
}

/// `expected_kind`: `None` when the caller omitted `kind` (no check, parity
/// with canonical's own optional discriminator); `Some(_)` enforces an
/// exact-parity mismatch check against the resolved record's actual
/// substrate/specific kind (ADR-099 B3 fix round 5, finding 1), mirroring
/// `handle_delete`'s `entity.kind != *expected` / `note.kind != *expected`
/// checks (update.rs:319, :348).
pub async fn prepare_delete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    expected_kind: Option<AtomicDeleteKind>,
) -> RuntimeResult<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;
    let hard = obj(args)?
        .get("hard")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // codex r2 High finding 2: `delete(id, hard=true)` is the public purge
    // route AFTER a prior soft delete (operations.rs hard-delete resolves
    // INCLUDING already-tombstoned rows). Live-only `resolve_by_id` would
    // never find an already-soft-deleted record, so hard delete must resolve
    // through the including-deleted variant; soft delete keeps the live-only
    // resolve (a soft delete of an already-tombstoned row is a no-op,
    // matching non-atomic behavior).
    let resolved = if hard {
        runtime.resolve_by_id_including_deleted(token, id).await?
    } else {
        runtime.resolve_by_id(token, id).await?
    };

    match resolved {
        Some(Resolved::Entity(entity)) => {
            match &expected_kind {
                None => {}
                Some(AtomicDeleteKind::Entity {
                    specific: Some(expected),
                }) => {
                    if &entity.kind != expected {
                        return Err(RuntimeError::NotFound(format!("{expected} {id}")));
                    }
                }
                Some(AtomicDeleteKind::Entity { specific: None }) => {}
                Some(AtomicDeleteKind::Note { .. }) => {
                    return Err(RuntimeError::NotFound(format!("note {id}")));
                }
                Some(AtomicDeleteKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            let namespace = entity.namespace.clone();
            // Storage parity: `entity_soft_delete_statement`/
            // `entity_hard_delete_statement` are the SAME khive-db builders
            // khive-db's own `SqlEntityStore::delete_entity` calls — no DML
            // text is hand-duplicated here.
            let mut statements = if hard {
                vec![PlanStatement {
                    statement: entity_hard_delete_statement(id),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: entity_soft_delete_statement(id, deleted_at),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                // Same builder canonical `delete_entity`'s hard-delete
                // cascade calls (`graph.purge_incident_edges`).
                statements.push(PlanStatement {
                    statement: purge_incident_edges_statement(id),
                    guard: None,
                });
            }
            // FTS + vector index purge (operations.rs delete_entity parity,
            // codex REJECT Blocker 1): both soft AND hard delete clean
            // indexes (hard delete of an already-tombstoned record must
            // still purge indexes — Finding 2); only hard additionally
            // cascades edges above.
            push_index_purge_statements(
                runtime,
                &mut statements,
                "fts_entities",
                &namespace,
                id,
                "atomic-delete-entity",
            )
            .await?;
            // GAP-1 (B3 fix round): operations.rs's `delete_entity` appends
            // an `EntityDeleted` event after a successful row delete, on
            // BOTH soft and hard delete (`if deleted { append_event(...) }`,
            // where `deleted` is true whenever the guarded row statement
            // above affected a row — `apply_plan` never reaches this
            // statement otherwise, so no `if` is needed here).
            statements.extend(event_append_statements(
                &namespace,
                "delete",
                EventKind::EntityDeleted,
                SubstrateKind::Entity,
                id,
                serde_json::json!({"id": id, "namespace": namespace, "hard": hard}),
            )?);
            Ok(AtomicOpPlan::Delete(DeletePlan {
                target_id: id,
                statements,
            }))
        }
        Some(Resolved::Note(note)) => {
            match &expected_kind {
                None => {}
                Some(AtomicDeleteKind::Note {
                    specific: Some(expected),
                }) => {
                    if &note.kind != expected {
                        return Err(RuntimeError::NotFound(format!("{expected} {id}")));
                    }
                }
                Some(AtomicDeleteKind::Note { specific: None }) => {}
                Some(AtomicDeleteKind::Entity { .. }) => {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
                Some(AtomicDeleteKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            let namespace = note.namespace.clone();
            // Storage parity: `note_soft_delete_statement`/
            // `note_hard_delete_statement` are the SAME khive-db builders
            // khive-db's own `SqlNoteStore::delete_note` calls.
            let mut statements = if hard {
                vec![PlanStatement {
                    statement: note_hard_delete_statement(id),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: note_soft_delete_statement(id, deleted_at),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.push(PlanStatement {
                    statement: purge_incident_edges_statement(id),
                    guard: None,
                });
            }
            // FTS + vector index purge (operations.rs delete_note parity,
            // codex REJECT Blocker 1): both soft AND hard delete clean
            // indexes (hard delete of an already-tombstoned record must
            // still purge indexes — Finding 2); only hard additionally
            // cascades edges above.
            push_index_purge_statements(
                runtime,
                &mut statements,
                "fts_notes",
                &namespace,
                id,
                "atomic-delete-note",
            )
            .await?;
            // GAP-1 (B3 fix round): operations.rs's `delete_note` appends a
            // `NoteDeleted` event after a successful row delete, on BOTH
            // soft and hard delete — same reasoning as the entity branch
            // above.
            statements.extend(event_append_statements(
                &namespace,
                "delete",
                EventKind::NoteDeleted,
                SubstrateKind::Note,
                id,
                serde_json::json!({"id": id, "namespace": namespace, "hard": hard}),
            )?);
            Ok(AtomicOpPlan::Delete(DeletePlan {
                target_id: id,
                statements,
            }))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "delete target {id} must be an entity, note, or edge"
        ))),
        // `Resolved` has no `Edge` variant (same reasoning as
        // `prepare_update`'s fallback above) — probe the graph store
        // directly. ADR-099 B3 r6: closes the round-4 codex REJECT (High).
        None => match &expected_kind {
            Some(AtomicDeleteKind::Entity { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicDeleteKind::Note { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicDeleteKind::Edge) | None => {
                let edge = if hard {
                    runtime.get_edge_including_deleted(token, id).await?
                } else {
                    runtime.get_edge(token, id).await?
                };
                match edge {
                    Some(edge) => prepare_delete_edge(id, edge, hard).await,
                    None => Err(RuntimeError::NotFound(format!("entity/note/edge {id}"))),
                }
            }
        },
    }
}

/// Edge branch of `prepare_delete` (ADR-099 B3 r6). Mirrors
/// `khive-runtime::operations::KhiveRuntime::delete_edge` exactly: hard
/// delete cascades `purge_incident_edges` (any `annotates` edge — or any
/// other edge — pointing AT this edge as a node) BEFORE deleting the edge
/// row itself, then a soft or hard delete statement, then an unconditional
/// `EdgeDeleted` event (edges are never FTS/vector-indexed, so unlike the
/// entity/note branches there is no index purge here — `delete_edge` has
/// none either).
async fn prepare_delete_edge(
    id: Uuid,
    edge: khive_storage::types::Edge,
    hard: bool,
) -> RuntimeResult<AtomicOpPlan> {
    let namespace = edge.namespace.clone();
    let mut statements: Vec<PlanStatement> = Vec::new();

    if hard {
        // Mirrors `delete_edge`'s `graph.purge_incident_edges(edge_id)` —
        // unguarded: zero incident edges is a legitimate outcome, not a
        // failure (same reasoning as the entity/note cascade-edges
        // statements above).
        statements.push(PlanStatement {
            statement: purge_incident_edges_statement(id),
            guard: None,
        });
        statements.push(PlanStatement {
            statement: edge_hard_delete_statement(id),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    } else {
        let now = chrono::Utc::now().timestamp_micros();
        statements.push(PlanStatement {
            statement: edge_soft_delete_statement(id, now),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }

    statements.extend(event_append_statements(
        &namespace,
        "delete",
        EventKind::EdgeDeleted,
        SubstrateKind::Entity,
        id,
        serde_json::json!({"id": id, "namespace": namespace, "hard": hard}),
    )?);

    Ok(AtomicOpPlan::Delete(DeletePlan {
        target_id: id,
        statements,
    }))
}

// ---------------------------------------------------------------------------
// link
// ---------------------------------------------------------------------------

fn parse_edge_relation(raw: &str) -> RuntimeResult<EdgeRelation> {
    raw.parse::<EdgeRelation>()
        .map_err(|e| RuntimeError::InvalidInput(format!("unknown edge relation {raw:?}: {e}")))
}

async fn prepare_link(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let source_id = require_uuid(args, "source_id")?;
    let target_id = require_uuid(args, "target_id")?;
    let relation = parse_edge_relation(require_str(args, "relation")?)?;
    let weight = optional_f64(args, "weight")?.unwrap_or(1.0);
    let metadata = obj(args)?.get("metadata").cloned();

    // Top-level `dependency_kind` param merges into `metadata`: only fills
    // the key when metadata doesn't already carry one. Calls the SAME
    // `khive_runtime::merge_entry_metadata` `khive-pack-kg`'s canonical
    // `handle_link` calls — relocated down to this crate (ADR-099 B3 r6
    // second pass) so both sides depend on one function instead of each
    // maintaining their own copy.
    let mut metadata = crate::merge_entry_metadata(
        metadata,
        optional_str(args, "dependency_kind").map(String::from),
    )?;

    validate_edge_weight(weight)?;
    runtime
        .validate_edge_relation_endpoints(token, source_id, target_id, relation)
        .await?;

    let (canon_source, canon_target) = canonical_edge_endpoints(relation, source_id, target_id);

    // Endpoint-kind `dependency_kind` inference for `depends_on` edges
    // (codex REJECT High finding — operations.rs `link()` parity): only
    // applies when both endpoints resolve as entities and the key is still
    // absent after the top-level-param merge above. Runs against the
    // CANONICAL endpoints, exactly mirroring `KhiveRuntime::link`'s own
    // ordering (canonicalize, then infer).
    if relation == EdgeRelation::DependsOn {
        metadata = match (
            runtime.resolve_edge_endpoint(token, canon_source).await?,
            runtime.resolve_edge_endpoint(token, canon_target).await?,
        ) {
            (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                merge_dependency_kind(&src_e.kind, &tgt_e.kind, metadata)
            }
            _ => metadata,
        };
    }

    validate_edge_metadata(relation, metadata.as_ref())?;
    let edge_id = Uuid::new_v4();
    let namespace = token.namespace().as_str().to_string();
    let now = chrono::Utc::now().timestamp_micros();
    let metadata_str = metadata.map(|m| serde_json::to_string(&m).unwrap_or_default());

    // ADR-099 B3 r7 (codex r7 High finding 2): the guarded `INSERT ...
    // SELECT ... WHERE EXISTS(...)` shape is kept (it is load-bearing —
    // `LinkPlan`'s own doc comment records why: it re-probes both endpoints
    // INSIDE the transaction, closing the intra-batch hazard where an
    // earlier op in the SAME atomic unit, e.g. `delete(X, hard)`, could
    // invalidate this op's prepare-time endpoint validation before commit).
    // What changed: the conflict-arm SET list is no longer a second
    // hand-assembled literal — `edge_insert_guarded_by_endpoints_statement`
    // shares the SAME `EDGE_NATURAL_KEY_CONFLICT_SET` text
    // `edge_upsert_statement` (canonical `link`'s builder) uses, so the two
    // cannot silently diverge again (the prior bug: this atomic literal
    // never set `target_backend = excluded.target_backend`, so a re-link of
    // an edge carrying a cross-backend `target_backend` stamp behaved
    // differently under `--atomic`).
    let statement = edge_insert_guarded_by_endpoints_statement(
        &namespace,
        edge_id,
        canon_source,
        canon_target,
        relation,
        weight,
        now,
        metadata_str.as_deref(),
    );

    Ok(AtomicOpPlan::Link(LinkPlan {
        source_id: canon_source,
        target_id: canon_target,
        statement: PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        },
    }))
}

// ---------------------------------------------------------------------------
// merge (entity-only, ADR-099 B3 scope decision — see final report)
// ---------------------------------------------------------------------------

// NOTE (B3 fix round, Leo refinement 2026-07-07): full atomic-merge parity
// (field folding, survivor FTS/vector reindex, loser index purge, merge
// provenance, same-kind rejection) was drafted and unit-tested in this round,
// but was reverted in favor of deferring atomic `merge` entirely at the
// pre-runtime admissibility guard (`khive_types::pack::
// ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`, alongside `propose`/`review`/`withdraw`)
// — see the fix-round report for the full rationale. This function is
// therefore back to its pre-fix-round shape: it still produces a plan (kept
// for the existing direct-prepare test coverage below and as defense in
// depth), but the CLI's `--atomic` surface never reaches it, since
// `check_atomic_admissible` rejects `merge` before any runtime is built.
async fn prepare_merge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let into_id = require_uuid(args, "into_id")?;
    let from_id = require_uuid(args, "from_id")?;
    if into_id == from_id {
        return Err(RuntimeError::InvalidInput(
            "cannot merge an entity into itself".into(),
        ));
    }

    let entities = runtime.entities(token)?;
    entities
        .get_entity(into_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {into_id}")))?;
    entities
        .get_entity(from_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {from_id}")))?;

    let now = chrono::Utc::now().timestamp_micros();
    let rewires = vec![
        crate::atomic_plan::PlanPredicate {
            description: "source_id = :from".to_string(),
            statement: SqlStatement {
                sql: "UPDATE graph_edges SET source_id = ?1, updated_at = ?2 WHERE source_id = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Text(into_id.to_string()),
                    SqlValue::Integer(now),
                    SqlValue::Text(from_id.to_string()),
                ],
                label: Some("atomic-merge-rewire-source".to_string()),
            },
        },
        crate::atomic_plan::PlanPredicate {
            description: "target_id = :from".to_string(),
            statement: SqlStatement {
                sql: "UPDATE graph_edges SET target_id = ?1, updated_at = ?2 WHERE target_id = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Text(into_id.to_string()),
                    SqlValue::Integer(now),
                    SqlValue::Text(from_id.to_string()),
                ],
                label: Some("atomic-merge-rewire-target".to_string()),
            },
        },
    ];
    let lifecycle = vec![PlanStatement {
        statement: SqlStatement {
            sql: "UPDATE entities SET deleted_at = ?1, merged_into = ?2 \
                  WHERE id = ?3 AND deleted_at IS NULL"
                .to_string(),
            params: vec![
                SqlValue::Integer(now),
                SqlValue::Text(into_id.to_string()),
                SqlValue::Text(from_id.to_string()),
            ],
            label: Some("atomic-merge-tombstone-from-entity".to_string()),
        },
        guard: Some(AffectedRowGuard::exactly(1)),
    }];

    Ok(AtomicOpPlan::Merge(MergePlan {
        into_id,
        from_id,
        rewires,
        lifecycle,
    }))
}

// ---------------------------------------------------------------------------
// post-commit effects
// ---------------------------------------------------------------------------

/// Run every deferred [`PostCommitEffect`] after a committed atomic unit,
/// outside any transaction (ADR-099 D1 phase 3). Re-fetches each target's
/// now-committed row and reuses the existing `reindex_entity`/`reindex_note`
/// (FTS + embedding, same as the non-atomic path) for exact parity.
pub async fn apply_post_commit_effects(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: Vec<PostCommitEffect>,
) -> RuntimeResult<()> {
    for effect in effects {
        match effect {
            PostCommitEffect::None => {}
            PostCommitEffect::ReindexEntity { entity_id } => {
                if let Some(entity) = runtime.entities(token)?.get_entity(entity_id).await? {
                    runtime.reindex_entity(token, &entity).await?;
                }
            }
            PostCommitEffect::ReindexNote { note_id } => {
                if let Some(note) = runtime.notes(token)?.get_note(note_id).await? {
                    runtime.reindex_note(token, &note).await?;
                }
            }
            PostCommitEffect::GtdAudit { .. } => {
                // GAP-5 (B3 fix round 4): applied by the `kkernel` caller's
                // own post-commit pass, not here — `khive-pack-gtd` (owner
                // of `ensure_audit_schema`/`write_audit_record`) depends on
                // `khive-runtime`, not the other way around, so this crate
                // cannot act on the effect itself. See `PostCommitEffect::
                // GtdAudit`'s doc comment.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use serde_json::json;

    use khive_types::Namespace;

    use crate::embedder_registry::EmbedderProvider;
    use crate::runtime::RuntimeConfig;

    const STUB_MODEL: &str = "stub-adr099-b3";
    const STUB_DIMS: usize = 4;

    struct StubService;

    #[async_trait]
    impl EmbeddingService for StubService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0.5_f32; STUB_DIMS]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            STUB_MODEL
        }
    }

    struct StubProvider;

    #[async_trait]
    impl EmbedderProvider for StubProvider {
        fn name(&self) -> &str {
            STUB_MODEL
        }

        fn dimensions(&self) -> usize {
            STUB_DIMS
        }

        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(StubService))
        }
    }

    fn scratch_runtime() -> KhiveRuntime {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("atomic_prepare_reindex.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        std::mem::forget(dir);
        rt
    }

    /// GAP-4 (B3 fix round 4): atomic `update` must reject a field that
    /// does not apply to the resolved substrate — parity with
    /// `khive-pack-kg::handlers::update::reject_inapplicable_fields`
    /// (update.rs:195). The pre-fix atomic prepare silently ignored
    /// `salience` on an entity: it would set every entity field to its
    /// CURRENT value, bump `updated_at`, satisfy the `exactly(1)` guard,
    /// and commit — a spurious no-op reported as success.
    #[tokio::test]
    async fn atomic_update_entity_rejects_note_only_field_salience() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entity = khive_storage::Entity::new("local", "concept", "GapFourEntity");
        let entity_id = entity.id;
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(entity)
            .await
            .expect("seed entity");

        let err = prepare_update(
            &runtime,
            &token,
            &json!({"id": entity_id.to_string(), "salience": 0.9}),
            None,
        )
        .await
        .expect_err("salience on an entity must be rejected, not silently accepted");
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("salience") && msg.contains("not valid for an entity")),
            "expected an InvalidInput naming the offending field, got: {err:?}"
        );

        // A valid entity update (name/description/tags) must still work.
        let plan = prepare_update(
            &runtime,
            &token,
            &json!({
                "id": entity_id.to_string(),
                "name": "GapFourEntity-renamed",
                "description": "updated description",
                "tags": ["a", "b"],
            }),
            None,
        )
        .await
        .expect("a valid entity field set must still be accepted");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));
        let entity = runtime
            .get_entity(&token, entity_id)
            .await
            .expect("get_entity");
        assert_eq!(entity.name, "GapFourEntity-renamed");
    }

    /// GAP-4 (B3 fix round 4): symmetric note-substrate case — `description`
    /// and `tags` are entity-only fields; passing either for a note must be
    /// rejected the same way update.rs rejects them.
    #[tokio::test]
    async fn atomic_update_note_rejects_entity_only_field_description() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let mut note = khive_storage::note::Note::new("local", "observation", "gap-4 note content");
        note.name = Some("gap-four-note".to_string());
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let err = prepare_update(
            &runtime,
            &token,
            &json!({"id": note_id.to_string(), "description": "entities have descriptions, notes don't"}),
                None,
            )
        .await
        .expect_err("description on a note must be rejected, not silently accepted");
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("description") && msg.contains("not valid for a note")),
            "expected an InvalidInput naming the offending field, got: {err:?}"
        );

        // A valid note update (content) must still work.
        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": note_id.to_string(), "content": "gap-4 note content, revised"}),
            None,
        )
        .await
        .expect("a valid note field must still be accepted");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));
    }

    /// ADR-099 B3 acceptance test 3: updating a note's content inside an
    /// atomic unit must, after commit, leave the note recallable via FTS
    /// under its NEW content and its vector row refreshed — parity with the
    /// non-atomic `update_note` -> `reindex_note` path.
    #[tokio::test]
    async fn atomic_update_note_content_is_fts_and_vector_reindexed_post_commit() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let mut note = khive_storage::note::Note::new("local", "observation", "original content");
        note.name = Some("reindex-target".to_string());
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed note");

        // Sanity: no vector row yet for the stub model.
        let vec_store = runtime
            .vectors_for_model(&token, STUB_MODEL)
            .expect("vec store");
        assert_eq!(vec_store.count().await.expect("count before"), 0);

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": note_id.to_string(), "content": "freshly-updated-content-xyz"}),
            None,
        )
        .await
        .expect("prepare update");

        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        let post_commit = match outcome {
            crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected Committed, got {other:?}"),
        };
        assert_eq!(
            post_commit,
            vec![PostCommitEffect::ReindexNote { note_id }],
            "content change must schedule exactly one ReindexNote post-commit effect"
        );

        apply_post_commit_effects(&runtime, &token, post_commit)
            .await
            .expect("apply post-commit effects");

        // FTS: the note must be recallable under its NEW content.
        let doc = runtime
            .text_for_notes(&token)
            .expect("text store")
            .get_document("local", note_id)
            .await
            .expect("get_document")
            .expect("document must be indexed after post-commit reindex");
        assert!(
            doc.body.contains("freshly-updated-content-xyz"),
            "FTS body must reflect the committed content: {:?}",
            doc.body
        );

        // Vector: a row must now exist for the registered stub model.
        assert_eq!(
            vec_store.count().await.expect("count after"),
            1,
            "post-commit reindex must have inserted a vector row for the stub model"
        );
    }

    /// B3 fix round (codex REJECT Blocker 1): atomic delete must purge the
    /// note's FTS row and vector row for BOTH soft and hard delete — parity
    /// with `KhiveRuntime::delete_note`'s index-cleanup contract.
    #[tokio::test]
    async fn atomic_delete_note_purges_fts_and_vector_indexes_soft_and_hard() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        for hard in [false, true] {
            let mut note =
                khive_storage::note::Note::new("local", "observation", "purge-target content");
            note.name = Some(format!("purge-target-hard-{hard}"));
            let note_id = note.id;
            runtime
                .notes(&token)
                .expect("notes store")
                .upsert_note(note.clone())
                .await
                .expect("seed note");
            runtime
                .reindex_note(&token, &note)
                .await
                .expect("seed index rows");

            let vec_store = runtime
                .vectors_for_model(&token, STUB_MODEL)
                .expect("vec store");
            assert_eq!(
                vec_store.count().await.expect("count before"),
                1,
                "seeded note must have a vector row before delete (hard={hard})"
            );
            assert!(
                runtime
                    .text_for_notes(&token)
                    .expect("text store")
                    .get_document("local", note_id)
                    .await
                    .expect("get_document")
                    .is_some(),
                "seeded note must have an FTS row before delete (hard={hard})"
            );

            let plan = prepare_delete(
                &runtime,
                &token,
                &json!({"id": note_id.to_string(), "hard": hard}),
                None,
            )
            .await
            .expect("prepare delete");
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("seam call ok");
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected commit (hard={hard}): {outcome:?}"
            );

            assert!(
                runtime
                    .text_for_notes(&token)
                    .expect("text store")
                    .get_document("local", note_id)
                    .await
                    .expect("get_document")
                    .is_none(),
                "FTS row must be purged after atomic delete (hard={hard})"
            );
            assert_eq!(
                vec_store.count().await.expect("count after"),
                0,
                "vector row must be purged after atomic delete (hard={hard})"
            );
        }
    }

    /// B3 fix round (codex REJECT Blocker 1): atomic delete must purge the
    /// entity's FTS row and vector row for BOTH soft and hard delete — parity
    /// with `KhiveRuntime::delete_entity`'s index-cleanup contract.
    #[tokio::test]
    async fn atomic_delete_entity_purges_fts_and_vector_indexes_soft_and_hard() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        for hard in [false, true] {
            let entity =
                khive_storage::Entity::new("local", "concept", format!("purge-target-hard-{hard}"));
            let entity_id = entity.id;
            runtime
                .entities(&token)
                .expect("entities store")
                .upsert_entity(entity.clone())
                .await
                .expect("seed entity");
            runtime
                .reindex_entity(&token, &entity)
                .await
                .expect("seed index rows");

            let vec_store = runtime
                .vectors_for_model(&token, STUB_MODEL)
                .expect("vec store");
            assert_eq!(
                vec_store.count().await.expect("count before"),
                1,
                "seeded entity must have a vector row before delete (hard={hard})"
            );
            assert!(
                runtime
                    .text(&token)
                    .expect("text store")
                    .get_document("local", entity_id)
                    .await
                    .expect("get_document")
                    .is_some(),
                "seeded entity must have an FTS row before delete (hard={hard})"
            );

            let plan = prepare_delete(
                &runtime,
                &token,
                &json!({"id": entity_id.to_string(), "hard": hard}),
                None,
            )
            .await
            .expect("prepare delete");
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("seam call ok");
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected commit (hard={hard}): {outcome:?}"
            );

            assert!(
                runtime
                    .text(&token)
                    .expect("text store")
                    .get_document("local", entity_id)
                    .await
                    .expect("get_document")
                    .is_none(),
                "FTS row must be purged after atomic delete (hard={hard})"
            );
            assert_eq!(
                vec_store.count().await.expect("count after"),
                0,
                "vector row must be purged after atomic delete (hard={hard})"
            );
        }
    }

    /// B3 fix round (codex REJECT High finding): atomic link must persist an
    /// explicit top-level `dependency_kind` param into edge metadata, and
    /// must infer one for `depends_on` edges when absent — parity with
    /// `link.rs`'s `merge_entry_metadata` and `operations.rs`'s
    /// `infer_dependency_kind` table.
    #[tokio::test]
    async fn atomic_link_persists_explicit_dependency_kind_and_infers_when_absent() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");

        fn metadata_json(plan: &AtomicOpPlan) -> String {
            let link_plan = match plan {
                AtomicOpPlan::Link(p) => p,
                other => panic!("expected an AtomicOpPlan::Link, got {other:?}"),
            };
            match link_plan.statement.statement.params.last() {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("expected the metadata param to be SqlValue::Text, got {other:?}"),
            }
        }

        // (a) explicit top-level `dependency_kind` param persists in metadata.
        {
            let svc = khive_storage::Entity::new("local", "service", "SvcA");
            let proj = khive_storage::Entity::new("local", "project", "ProjB");
            let (svc_id, proj_id) = (svc.id, proj.id);
            entities.upsert_entity(svc).await.expect("seed svc");
            entities.upsert_entity(proj).await.expect("seed proj");

            let plan = prepare_link(
                &runtime,
                &token,
                &json!({
                    "source_id": svc_id.to_string(),
                    "target_id": proj_id.to_string(),
                    "relation": "depends_on",
                    "dependency_kind": "artifact",
                }),
            )
            .await
            .expect("prepare link");
            let json_str = metadata_json(&plan);
            assert!(
                json_str.contains(r#""dependency_kind":"artifact""#),
                "explicit dependency_kind param must persist: {json_str}"
            );
        }

        // (b) `depends_on` with no explicit dependency_kind infers from
        // endpoint kinds: (service, service) -> "runtime".
        {
            let svc_a = khive_storage::Entity::new("local", "service", "SvcC");
            let svc_b = khive_storage::Entity::new("local", "service", "SvcD");
            let (a_id, b_id) = (svc_a.id, svc_b.id);
            entities.upsert_entity(svc_a).await.expect("seed svc a");
            entities.upsert_entity(svc_b).await.expect("seed svc b");

            let plan = prepare_link(
                &runtime,
                &token,
                &json!({
                    "source_id": a_id.to_string(),
                    "target_id": b_id.to_string(),
                    "relation": "depends_on",
                }),
            )
            .await
            .expect("prepare link");
            let json_str = metadata_json(&plan);
            assert!(
                json_str.contains(r#""dependency_kind":"runtime""#),
                "inferred dependency_kind for (service, service) must persist: {json_str}"
            );
        }
    }

    /// Raw natural-key probe of `graph_edges` (namespace, source_id,
    /// target_id, relation) — returns `(weight, metadata_json, deleted_at)`
    /// for exactly the ONE row a UNIQUE(namespace, source_id, target_id,
    /// relation) constraint permits. `None` means no row at all.
    async fn probe_edge_natural_key(
        runtime: &KhiveRuntime,
        namespace: &str,
        source_id: Uuid,
        target_id: Uuid,
        relation: &str,
    ) -> (usize, Option<f64>, Option<String>, Option<i64>) {
        let mut reader = runtime.sql().reader().await.expect("reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT weight, metadata, deleted_at FROM graph_edges \
                      WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 AND relation = ?4"
                    .to_string(),
                params: vec![
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text(source_id.to_string()),
                    SqlValue::Text(target_id.to_string()),
                    SqlValue::Text(relation.to_string()),
                ],
                label: Some("test-probe-edge-natural-key".to_string()),
            })
            .await
            .expect("probe edge natural key");
        let count = rows.len();
        let Some(row) = rows.into_iter().next() else {
            return (count, None, None, None);
        };
        let weight = match row.get("weight") {
            Some(SqlValue::Float(f)) => Some(*f),
            Some(SqlValue::Integer(i)) => Some(*i as f64),
            _ => None,
        };
        let metadata = match row.get("metadata") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        };
        let deleted_at = match row.get("deleted_at") {
            Some(SqlValue::Integer(i)) => Some(*i),
            _ => None,
        };
        (count, weight, metadata, deleted_at)
    }

    /// GAP-2 (B3 fix round 4): atomic `link` must be an upsert, exactly like
    /// canonical `link` -> `upsert_edge`'s natural-key `ON CONFLICT` arm —
    /// re-linking an already-linked triple must SUCCEED and update
    /// weight/metadata, not hit the `UNIQUE(namespace, source_id, target_id,
    /// relation)` constraint and roll back the whole atomic unit.
    #[tokio::test]
    async fn atomic_link_of_already_linked_triple_upserts_weight_and_metadata() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");

        let a = khive_storage::Entity::new("local", "concept", "GapTwoA");
        let b = khive_storage::Entity::new("local", "concept", "GapTwoB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");

        // (a) a fresh link still works.
        let plan1 = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": a_id.to_string(),
                "target_id": b_id.to_string(),
                "relation": "extends",
                "weight": 0.5,
            }),
        )
        .await
        .expect("prepare first link");
        let outcome1 = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan1])
            .await
            .expect("seam call ok");
        assert!(
            matches!(
                outcome1,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "fresh link must commit: {outcome1:?}"
        );
        let (count, weight, _metadata, deleted_at) =
            probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
        assert_eq!(count, 1, "exactly one edge row after the fresh link");
        assert_eq!(weight, Some(0.5));
        assert!(deleted_at.is_none());

        // (b) re-linking the SAME triple with a different weight/metadata
        // must SUCCEED (not a constraint-violation rollback) and UPDATE the
        // existing row in place — natural key stays unique.
        let plan2 = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": a_id.to_string(),
                "target_id": b_id.to_string(),
                "relation": "extends",
                "weight": 0.9,
                "metadata": {"note": "relinked"},
            }),
        )
        .await
        .expect("prepare second link");
        let outcome2 = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan2])
            .await
            .expect("seam call ok");
        assert!(
            matches!(
                outcome2,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "re-link of an already-linked triple must upsert, not roll back: {outcome2:?}"
        );
        let (count, weight, metadata, deleted_at) =
            probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
        assert_eq!(
            count, 1,
            "the natural-key UNIQUE constraint must still hold exactly one row (upsert, not a second insert)"
        );
        assert_eq!(weight, Some(0.9), "weight must be updated to the new value");
        assert!(
            metadata
                .as_deref()
                .is_some_and(|m| m.contains(r#""note":"relinked""#)),
            "metadata must be updated to the new value: {metadata:?}"
        );
        assert!(deleted_at.is_none());
    }

    /// GAP-2 (B3 fix round 4): atomic `link` of a SOFT-DELETED triple must
    /// resurrect it (`deleted_at = NULL`), matching `upsert_edge`'s
    /// natural-key `ON CONFLICT ... DO UPDATE SET deleted_at = NULL`.
    #[tokio::test]
    async fn atomic_link_of_soft_deleted_triple_resurrects_it() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");

        let a = khive_storage::Entity::new("local", "concept", "GapTwoResurrectA");
        let b = khive_storage::Entity::new("local", "concept", "GapTwoResurrectB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");

        let plan = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": a_id.to_string(),
                "target_id": b_id.to_string(),
                "relation": "extends",
            }),
        )
        .await
        .expect("prepare link");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));

        // Soft-delete the edge row directly (natural-key UPDATE — mirrors
        // what `delete_edge(hard=false)` does to this same row).
        {
            let mut writer = runtime.sql().writer().await.expect("writer");
            let affected = writer
                .execute(SqlStatement {
                    sql: "UPDATE graph_edges SET deleted_at = ?1 \
                          WHERE namespace = ?2 AND source_id = ?3 AND target_id = ?4 AND relation = ?5"
                        .to_string(),
                    params: vec![
                        SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
                        SqlValue::Text("local".to_string()),
                        SqlValue::Text(a_id.to_string()),
                        SqlValue::Text(b_id.to_string()),
                        SqlValue::Text("extends".to_string()),
                    ],
                    label: Some("test-soft-delete-edge".to_string()),
                })
                .await
                .expect("soft delete edge");
            assert_eq!(affected, 1, "soft-delete must touch exactly the seeded row");
        }
        let (_, _, _, deleted_at) =
            probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
        assert!(
            deleted_at.is_some(),
            "row must be soft-deleted before the resurrect attempt"
        );

        // Re-link the same triple: must resurrect (deleted_at -> NULL), not
        // fail on the UNIQUE constraint of the still-present soft-deleted row.
        let plan_relink = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": a_id.to_string(),
                "target_id": b_id.to_string(),
                "relation": "extends",
                "weight": 0.75,
            }),
        )
        .await
        .expect("prepare resurrecting link");
        let outcome_relink =
            crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan_relink])
                .await
                .expect("seam call ok");
        assert!(
            matches!(
                outcome_relink,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "re-linking a soft-deleted triple must resurrect it, not roll back: {outcome_relink:?}"
        );
        let (count, weight, _, deleted_at) =
            probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
        assert_eq!(count, 1);
        assert_eq!(weight, Some(0.75));
        assert!(
            deleted_at.is_none(),
            "re-link must resurrect the soft-deleted row (deleted_at -> NULL)"
        );
    }

    /// B3 fix round 3 (codex r2 Blocker 1): atomic delete of an entity AND a
    /// note must SUCCEED even when the registered embedding model's `vec_*`
    /// table has never been lazily created (a fresh DB registers models
    /// before any vector store is opened) — the raw purge DML must skip
    /// tables that don't exist rather than hit `no such table` and roll
    /// back the whole atomic unit. FTS purge still fires (those tables
    /// always exist) and the delete itself is a clean commit.
    #[tokio::test]
    async fn atomic_delete_succeeds_when_vec_table_never_created() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        // Seed via raw upsert ONLY — never call reindex_entity/reindex_note
        // or vectors_for_model, so the stub model's `vec_*` table is never
        // lazily created (opening the vector store is what creates it).
        let entity = khive_storage::Entity::new("local", "concept", "no-vec-table-entity");
        let entity_id = entity.id;
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(entity)
            .await
            .expect("seed entity");

        let mut note = khive_storage::note::Note::new("local", "observation", "no-vec-table-note");
        note.name = Some("no-vec-table-note".to_string());
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed note");

        for (id, kind) in [(entity_id, "entity"), (note_id, "note")] {
            let plan = prepare_delete(&runtime, &token, &json!({"id": id.to_string()}), None)
                .await
                .unwrap_or_else(|e| panic!("prepare delete ({kind}) must not fail: {e}"));
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .unwrap_or_else(|e| {
                    panic!("atomic delete ({kind}) must not hit `no such table`: {e}")
                });
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected a clean commit ({kind}): {outcome:?}"
            );
        }

        assert!(
            runtime
                .get_entity_including_deleted(&token, entity_id)
                .await
                .expect("get entity")
                .expect("entity row still present (soft delete)")
                .deleted_at
                .is_some(),
            "entity must be soft-deleted"
        );
        assert!(
            runtime
                .get_note_including_deleted(&token, note_id)
                .await
                .expect("get note")
                .expect("note row still present (soft delete)")
                .deleted_at
                .is_some(),
            "note must be soft-deleted"
        );
    }

    /// B3 fix round 3 (codex r2 High finding 2): atomic hard delete must be
    /// able to purge a record that was ALREADY soft-deleted — parity with
    /// `delete(id, hard=true)` being the public purge route after a prior
    /// soft delete (the non-atomic hard path resolves including deleted
    /// rows and its DML carries no `deleted_at` predicate).
    #[tokio::test]
    async fn atomic_hard_delete_purges_already_soft_deleted_entity_and_note() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let entity =
            khive_storage::Entity::new("local", "concept", "tombstoned-entity-hard-delete");
        let entity_id = entity.id;
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(entity.clone())
            .await
            .expect("seed entity");
        runtime
            .reindex_entity(&token, &entity)
            .await
            .expect("seed index rows");

        let mut note =
            khive_storage::note::Note::new("local", "observation", "tombstoned-note-hard-delete");
        note.name = Some("tombstoned-note-hard-delete".to_string());
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note.clone())
            .await
            .expect("seed note");
        runtime
            .reindex_note(&token, &note)
            .await
            .expect("seed index rows");

        // First: SOFT delete both (via atomic prepare) so they're tombstoned
        // going into the hard-delete attempt below.
        for id in [entity_id, note_id] {
            let plan = prepare_delete(&runtime, &token, &json!({"id": id.to_string()}), None)
                .await
                .expect("prepare soft delete");
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("soft delete commit");
            assert!(matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ));
        }
        assert!(
            runtime
                .get_entity_including_deleted(&token, entity_id)
                .await
                .expect("get entity")
                .expect("entity present")
                .deleted_at
                .is_some(),
            "entity must be soft-deleted before the hard-delete attempt"
        );
        assert!(
            runtime
                .get_note_including_deleted(&token, note_id)
                .await
                .expect("get note")
                .expect("note present")
                .deleted_at
                .is_some(),
            "note must be soft-deleted before the hard-delete attempt"
        );

        // Now: HARD delete the already-tombstoned records.
        for (id, kind) in [(entity_id, "entity"), (note_id, "note")] {
            let plan = prepare_delete(
                &runtime,
                &token,
                &json!({"id": id.to_string(), "hard": true}),
                None,
            )
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "prepare hard delete ({kind}) of an already-soft-deleted record \
                         must resolve it: {e}"
                )
            });
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .unwrap_or_else(|e| panic!("hard delete ({kind}) commit failed: {e}"));
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected a clean hard-delete commit ({kind}): {outcome:?}"
            );
        }

        assert!(
            runtime
                .get_entity_including_deleted(&token, entity_id)
                .await
                .expect("get entity")
                .is_none(),
            "entity row must be fully purged after hard delete"
        );
        assert!(
            runtime
                .get_note_including_deleted(&token, note_id)
                .await
                .expect("get note")
                .is_none(),
            "note row must be fully purged after hard delete"
        );
        assert!(
            runtime
                .text(&token)
                .expect("text store")
                .get_document("local", entity_id)
                .await
                .expect("get_document")
                .is_none(),
            "entity FTS row must be purged after hard delete"
        );
        assert!(
            runtime
                .text_for_notes(&token)
                .expect("text store")
                .get_document("local", note_id)
                .await
                .expect("get_document")
                .is_none(),
            "note FTS row must be purged after hard delete"
        );
        let vec_store = runtime
            .vectors_for_model(&token, STUB_MODEL)
            .expect("vec store");
        assert_eq!(
            vec_store.count().await.expect("count after"),
            0,
            "vector rows for both records must be purged after hard delete"
        );
    }

    // ------------------------------------------------------------------
    // GAP-1 (B3 fix round): event-store append parity
    // ------------------------------------------------------------------

    /// Fetch every event of `kind` targeting `target_id`, via the same
    /// `EventStore::query_events` surface `--atomic` callers would use to
    /// verify parity — not a raw SQL probe.
    async fn events_for_target(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        target_id: Uuid,
        kind: EventKind,
    ) -> Vec<khive_storage::Event> {
        let event_store = runtime.events(token).expect("event store");
        let filter = khive_storage::EventFilter {
            kinds: vec![kind],
            ..Default::default()
        };
        let page = event_store
            .query_events(filter, khive_storage::types::PageRequest::default())
            .await
            .expect("query_events");
        page.items
            .into_iter()
            .filter(|e| e.target_id == Some(target_id))
            .collect()
    }

    /// GAP-1: atomic `update(id=<entity>, name=...)` must append an
    /// `EntityUpdated` event, matching `curation::update_entity`
    /// (curation.rs:257-273) — the event is appended unconditionally after
    /// a successful row update, not only on the reindex-triggering subset.
    #[tokio::test]
    async fn atomic_update_entity_appends_entity_updated_event() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entity = khive_storage::Entity::new("local", "concept", "gap1-entity");
        let entity_id = entity.id;
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(entity)
            .await
            .expect("seed entity");

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": entity_id.to_string(), "name": "gap1-entity-renamed"}),
            None,
        )
        .await
        .expect("prepare update");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));

        let events = events_for_target(&runtime, &token, entity_id, EventKind::EntityUpdated).await;
        assert_eq!(
            events.len(),
            1,
            "expected exactly one EntityUpdated event for {entity_id}"
        );
        assert_eq!(events[0].namespace, "local");
        assert_eq!(events[0].payload["id"], json!(entity_id.to_string()));
        assert_eq!(
            events[0].payload["changed_fields"],
            json!(["name"]),
            "changed_fields must name exactly the patched fields"
        );
    }

    /// GAP-1: atomic soft AND hard delete of an entity must each append an
    /// `EntityDeleted` event, matching `operations::delete_entity`
    /// (operations.rs:3543-3558), which fires on both delete modes.
    #[tokio::test]
    async fn atomic_delete_entity_appends_entity_deleted_event_soft_and_hard() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        for hard in [false, true] {
            let entity =
                khive_storage::Entity::new("local", "concept", format!("gap1-entity-hard-{hard}"));
            let entity_id = entity.id;
            runtime
                .entities(&token)
                .expect("entities store")
                .upsert_entity(entity)
                .await
                .expect("seed entity");

            let args = if hard {
                json!({"id": entity_id.to_string(), "hard": true})
            } else {
                json!({"id": entity_id.to_string()})
            };
            let plan = prepare_delete(&runtime, &token, &args, None)
                .await
                .unwrap_or_else(|e| panic!("prepare delete (hard={hard}): {e}"));
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .unwrap_or_else(|e| panic!("delete commit (hard={hard}): {e}"));
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected a clean delete commit (hard={hard}): {outcome:?}"
            );

            let events =
                events_for_target(&runtime, &token, entity_id, EventKind::EntityDeleted).await;
            assert_eq!(
                events.len(),
                1,
                "expected exactly one EntityDeleted event for {entity_id} (hard={hard})"
            );
            assert_eq!(events[0].payload["hard"], json!(hard));
        }
    }

    /// GAP-1: atomic soft AND hard delete of a note must each append a
    /// `NoteDeleted` event, matching `operations::delete_note`
    /// (operations.rs:3326-3340), which fires on both delete modes.
    #[tokio::test]
    async fn atomic_delete_note_appends_note_deleted_event_soft_and_hard() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        for hard in [false, true] {
            let mut note = khive_storage::note::Note::new(
                "local",
                "observation",
                format!("gap1-note-content-hard-{hard}"),
            );
            note.name = Some(format!("gap1-note-hard-{hard}"));
            let note_id = note.id;
            runtime
                .notes(&token)
                .expect("notes store")
                .upsert_note(note)
                .await
                .expect("seed note");

            let args = if hard {
                json!({"id": note_id.to_string(), "hard": true})
            } else {
                json!({"id": note_id.to_string()})
            };
            let plan = prepare_delete(&runtime, &token, &args, None)
                .await
                .unwrap_or_else(|e| panic!("prepare delete (hard={hard}): {e}"));
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .unwrap_or_else(|e| panic!("delete commit (hard={hard}): {e}"));
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected a clean delete commit (hard={hard}): {outcome:?}"
            );

            let events = events_for_target(&runtime, &token, note_id, EventKind::NoteDeleted).await;
            assert_eq!(
                events.len(),
                1,
                "expected exactly one NoteDeleted event for {note_id} (hard={hard})"
            );
            assert_eq!(events[0].payload["hard"], json!(hard));
        }
    }

    /// ADR-099 B3 r6 (closes the round-4 codex REJECT, High): `update`
    /// admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`; this asserts
    /// `prepare_update` actually builds a plan for one — a non-symmetric
    /// relation (`extends`) exercises the `edge_upsert_statement` reuse
    /// branch — and that the committed row + `EdgeUpdated` event match
    /// canonical `update_edge`'s shape (weight persisted, relation
    /// unchanged, exactly one event).
    #[tokio::test]
    async fn atomic_update_edge_patches_weight_and_appends_edge_updated_event() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", "GapEdgeA");
        let b = khive_storage::Entity::new("local", "concept", "GapEdgeB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");

        let edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::Extends, 0.4, None)
            .await
            .expect("seed edge");
        let edge_id = Uuid::from(edge.id);

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": edge_id.to_string(), "weight": 0.75}),
            None,
        )
        .await
        .expect("prepare update edge");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean edge update commit: {outcome:?}"
        );

        let updated = runtime
            .get_edge(&token, edge_id)
            .await
            .expect("get_edge")
            .expect("edge still present");
        assert_eq!(updated.weight, 0.75, "weight patch must persist");
        assert_eq!(updated.relation, EdgeRelation::Extends);

        let events = events_for_target(&runtime, &token, edge_id, EventKind::EdgeUpdated).await;
        assert_eq!(
            events.len(),
            1,
            "expected exactly one EdgeUpdated event for {edge_id}"
        );
        assert_eq!(
            events[0].payload["changed_fields"],
            json!(["weight"]),
            "changed_fields must name exactly the patched field"
        );
    }

    /// ADR-099 B3 r6: the symmetric-relation conflict-absorption branch of
    /// `prepare_update_edge` — mirrors `update_edge_symmetric_dml`'s case
    /// (b): changing a non-symmetric edge's `relation` to a symmetric one
    /// whose canonical natural key collides with an ALREADY-EXISTING
    /// symmetric edge between the same two entities must delete the
    /// requested (non-canonical) row and refresh the surviving canonical
    /// row in place, rather than raising a uniqueness error.
    #[tokio::test]
    async fn atomic_update_edge_symmetric_conflict_absorbs_into_surviving_row() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", "GapEdgeSymA");
        let b = khive_storage::Entity::new("local", "concept", "GapEdgeSymB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");

        // The non-canonical edge under test: A -> B, non-symmetric relation.
        let requested_edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
            .await
            .expect("seed requested edge");
        let requested_id = Uuid::from(requested_edge.id);

        // The pre-existing canonical row this update will collide with once
        // `relation` becomes `competes_with` (symmetric).
        let canonical_edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::CompetesWith, 0.6, None)
            .await
            .expect("seed canonical edge");
        let canonical_id = Uuid::from(canonical_edge.id);
        assert_ne!(requested_id, canonical_id);

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": requested_id.to_string(), "relation": "competes_with", "weight": 0.9}),
            None,
        )
        .await
        .expect("prepare update edge (symmetric conflict)");
        // ADR-099 B3 r9: the plan no longer computes a prepare-time
        // advisory surviving id (`target_id` is just the requested id) —
        // it carries `edge_natural_key` so a post-commit caller can derive
        // the real surviving id itself. Assert the plan carries the RIGHT
        // natural key to look up; the actual surviving row's identity is
        // verified against the DB after commit, below.
        let (canon_src, canon_tgt) =
            canonical_edge_endpoints(EdgeRelation::CompetesWith, a_id, b_id);
        match &plan {
            AtomicOpPlan::Update(p) => {
                assert_eq!(p.target_id, requested_id);
                let key = p
                    .edge_natural_key
                    .as_ref()
                    .expect("symmetric edge update must carry edge_natural_key");
                assert_eq!(key.canon_source_id, canon_src);
                assert_eq!(key.canon_target_id, canon_tgt);
                assert_eq!(key.relation, EdgeRelation::CompetesWith);
            }
            other => panic!("expected an Update plan, got {other:?}"),
        }

        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean symmetric-conflict-absorption commit: {outcome:?}"
        );

        // The requested (non-canonical) row must be gone.
        let requested_after = runtime
            .get_edge_including_deleted(&token, requested_id)
            .await
            .expect("get_edge_including_deleted");
        assert!(
            requested_after.is_none(),
            "the non-canonical requested row must be deleted, not just tombstoned"
        );

        // The surviving canonical row must carry the patch.
        let surviving = runtime
            .get_edge(&token, canonical_id)
            .await
            .expect("get_edge")
            .expect("surviving canonical row must remain");
        assert_eq!(surviving.weight, 0.9);
        assert_eq!(surviving.relation, EdgeRelation::CompetesWith);

        // Event target is the CALLER-supplied id, not the surviving id —
        // mirrors `update_edge`'s event using `edge_id` (the caller's
        // original argument), not the post-absorption id.
        let events =
            events_for_target(&runtime, &token, requested_id, EventKind::EdgeUpdated).await;
        assert_eq!(events.len(), 1);
    }

    /// ADR-099 B3 r9 (codex r8 Blocker finding 1): the same-unit race codex
    /// named — `[delete(X), update(X -> competes_with)]` where an
    /// ALREADY-EXISTING canonical row sits at the post-update natural key.
    /// Both ops' async prepare passes run before either commits, so at
    /// prepare time `X` still exists and both plans build. At commit time
    /// `delete(X)` removes it FIRST; `update(X -> competes_with)`'s own
    /// commit-time statements must then fail loud (its target no longer
    /// exists) rather than silently absorbing into the pre-existing
    /// canonical row it never causally touched. The whole atomic unit must
    /// roll back — parity with canonical `update_edge`'s `NotFound` for a
    /// missing edge, expressed here as the unit-level abort ADR-099
    /// specifies for any op whose commit-time guard fails.
    #[tokio::test]
    async fn atomic_update_edge_symmetric_same_unit_delete_race_aborts_the_unit() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", "GapEdgeRaceA");
        let b = khive_storage::Entity::new("local", "concept", "GapEdgeRaceB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");

        // The row op 1 will try to update — deleted by op 0 in the SAME unit.
        let requested_edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
            .await
            .expect("seed requested edge");
        let requested_id = Uuid::from(requested_edge.id);

        // The pre-existing canonical row the buggy `id = ?2 OR natural-key`
        // predicate used to silently absorb into.
        let canonical_edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::CompetesWith, 0.6, None)
            .await
            .expect("seed canonical edge");
        let canonical_id = Uuid::from(canonical_edge.id);

        let delete_plan = prepare_delete(
            &runtime,
            &token,
            &json!({"id": requested_id.to_string(), "hard": true}),
            None,
        )
        .await
        .expect("prepare delete edge");
        let update_plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": requested_id.to_string(), "relation": "competes_with", "weight": 0.9}),
            None,
        )
        .await
        .expect("prepare update edge (both prepares run before either commits)");

        let outcome = crate::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![delete_plan, update_plan],
        )
        .await
        .expect("the seam call itself must not error — the unit rolls back cleanly");
        match outcome {
            crate::atomic_runner::AtomicRunOutcome::RolledBack {
                failed_op_index, ..
            } => {
                assert_eq!(
                    failed_op_index, 1,
                    "op 1 (the update) must be the one whose guard fails"
                );
            }
            other => panic!("expected the whole unit to roll back, got {other:?}"),
        }

        // Whole-unit rollback: op 0's delete must be undone too.
        let requested_after = runtime
            .get_edge(&token, requested_id)
            .await
            .expect("get_edge");
        assert!(
            requested_after.is_some(),
            "delete(X) must have rolled back along with the failed update"
        );
        // The pre-existing canonical row must be completely untouched.
        let canonical_after = runtime
            .get_edge(&token, canonical_id)
            .await
            .expect("get_edge")
            .expect("canonical row must still be present");
        assert_eq!(
            canonical_after.weight, 0.6,
            "the pre-existing canonical row must never have been touched by the aborted update"
        );
    }

    /// ADR-099 B3 r6 (closes the round-4 codex REJECT, High): `update`
    /// rejects an entity/note-only field (`name`) on an edge target,
    /// mirroring `khive-pack-kg::handlers::update::reject_inapplicable_fields`'s
    /// `KindSpec::Edge` arm — before this fix `reject_inapplicable_update_fields`
    /// had no `"edge"` match arm at all, so the field was silently dropped.
    #[tokio::test]
    async fn atomic_update_edge_rejects_entity_only_field_name() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", "GapEdgeRejectA");
        let b = khive_storage::Entity::new("local", "concept", "GapEdgeRejectB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");
        let edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::Extends, 0.5, None)
            .await
            .expect("seed edge");
        let edge_id = Uuid::from(edge.id);

        let err = prepare_update(
            &runtime,
            &token,
            &json!({"id": edge_id.to_string(), "name": "not-a-valid-edge-field"}),
            None,
        )
        .await
        .expect_err("edge update with an entity-only field must be rejected");
        let message = err.to_string();
        assert!(
            message.contains("name") && message.contains("edge"),
            "error must name the offending field and the substrate: {message}"
        );
    }

    /// ADR-099 B3 r6 (closes the round-4 codex REJECT, High): `delete`
    /// admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`; this asserts
    /// `prepare_delete` actually builds a plan for one on both soft and
    /// hard delete, matching `operations::delete_edge`'s row-mode DML and
    /// unconditional `EdgeDeleted` event.
    #[tokio::test]
    async fn atomic_delete_edge_soft_and_hard_appends_edge_deleted_event() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        for hard in [false, true] {
            let entities = runtime.entities(&token).expect("entities store");
            let a = khive_storage::Entity::new("local", "concept", format!("GapEdgeDelA{hard}"));
            let b = khive_storage::Entity::new("local", "concept", format!("GapEdgeDelB{hard}"));
            let (a_id, b_id) = (a.id, b.id);
            entities.upsert_entity(a).await.expect("seed a");
            entities.upsert_entity(b).await.expect("seed b");
            let edge = runtime
                .link(&token, a_id, b_id, EdgeRelation::Extends, 0.5, None)
                .await
                .expect("seed edge");
            let edge_id = Uuid::from(edge.id);

            let args = if hard {
                json!({"id": edge_id.to_string(), "hard": true})
            } else {
                json!({"id": edge_id.to_string()})
            };
            let plan = prepare_delete(&runtime, &token, &args, None)
                .await
                .unwrap_or_else(|e| panic!("prepare delete edge (hard={hard}): {e}"));
            let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .unwrap_or_else(|e| panic!("edge delete commit (hard={hard}): {e}"));
            assert!(
                matches!(
                    outcome,
                    crate::atomic_runner::AtomicRunOutcome::Committed { .. }
                ),
                "expected a clean edge delete commit (hard={hard}): {outcome:?}"
            );

            let after = runtime
                .get_edge_including_deleted(&token, edge_id)
                .await
                .expect("get_edge_including_deleted");
            if hard {
                assert!(after.is_none(), "hard delete must purge the row entirely");
            } else {
                assert!(
                    after.as_ref().is_some_and(|e| e.deleted_at.is_some()),
                    "soft delete must tombstone, not purge"
                );
            }

            let events = events_for_target(&runtime, &token, edge_id, EventKind::EdgeDeleted).await;
            assert_eq!(
                events.len(),
                1,
                "expected exactly one EdgeDeleted event for {edge_id} (hard={hard})"
            );
            assert_eq!(events[0].payload["hard"], json!(hard));
        }
    }

    /// GAP-1 parity boundary: atomic `update` of a NOTE must append NO
    /// event — canonical `update_note` never calls `append_event` (verified
    /// by the sweep; unlike `update_entity`, which always does).
    #[tokio::test]
    async fn atomic_update_note_appends_no_event() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let mut note = khive_storage::note::Note::new("local", "observation", "gap1-note-noevent");
        note.name = Some("gap1-note-noevent".to_string());
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": note_id.to_string(), "content": "gap1-note-noevent, revised"}),
            None,
        )
        .await
        .expect("prepare update");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));

        let event_store = runtime.events(&token).expect("event store");
        let page = event_store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::types::PageRequest::default(),
            )
            .await
            .expect("query_events");
        assert!(
            page.items.iter().all(|e| e.target_id != Some(note_id)),
            "update_note must append no event; found: {:?}",
            page.items
                .iter()
                .filter(|e| e.target_id == Some(note_id))
                .collect::<Vec<_>>()
        );
    }

    /// GAP-1 parity boundary: atomic `link` must append NO event —
    /// canonical `link` never calls `append_event` (verified by the sweep).
    #[tokio::test]
    async fn atomic_link_appends_no_event() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let source = khive_storage::Entity::new("local", "concept", "gap1-link-source");
        let target = khive_storage::Entity::new("local", "concept", "gap1-link-target");
        let (source_id, target_id) = (source.id, target.id);
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(source)
            .await
            .expect("seed source");
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(target)
            .await
            .expect("seed target");

        let plan = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": source_id.to_string(),
                "target_id": target_id.to_string(),
                "relation": "extends",
            }),
        )
        .await
        .expect("prepare link");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));

        let event_store = runtime.events(&token).expect("event store");
        let page = event_store
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::types::PageRequest::default(),
            )
            .await
            .expect("query_events");
        assert!(
            page.items.is_empty(),
            "link must append no event; found: {:?}",
            page.items
        );
    }
}
