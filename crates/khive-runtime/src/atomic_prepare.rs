//! ADR-099: the per-verb async prepare pass for the KG-substrate v1
//! admissible verbs (`update`, `delete`, `link`, `merge`), plus
//! [`prepare_add_entity`]/[`prepare_add_note`] for the ADR-046 proposal
//! changeset `AddEntity`/`AddNote` arms. Each `prepare_*` function reads
//! current state (async, outside any transaction) and returns a plain-data
//! [`crate::atomic_runner::AtomicOpPlan`] ([`crate::atomic_plan`]) for the
//! synchronous commit pass ([`crate::atomic_runner::run_atomic_unit`]) to
//! apply.
//!
//! `gtd.transition`/`gtd.complete` prepare is deliberately not here (lives in
//! `kkernel` instead), and `propose`/`review`/`withdraw`/`merge` are on the
//! v1 admissible list but have no working prepare implementation in this
//! module (`prepare_governance_unimplemented` fails loudly rather than
//! silently no-opping; `prepare_merge` is unreachable through `--atomic` and
//! kept only for its own tests and as defense in depth). See
//! `docs/atomic_prepare.md#module-scope` for why each of these is excluded
//! and what would be required to admit them.

use serde_json::Value;
use uuid::Uuid;

use khive_storage::types::SqlValue;
use khive_storage::{EdgeRelation, SqlStatement};
use khive_types::{EventKind, SubstrateKind};

use crate::atomic_plan::{
    AddEntityPlan, AddNotePlan, AffectedRowGuard, DeletePlan, EdgeNaturalKey, LinkPlan, MergePlan,
    PlanStatement, PostCommitEffect, UpdatePlan,
};
use crate::atomic_runner::AtomicOpPlan;
use crate::curation::{entity_fts_document, note_fts_document};
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
use khive_db::stores::text::insert_document_statement;

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

fn optional_create_string(args: &Value, key: &str) -> RuntimeResult<Option<String>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// Nullable-string patch semantics mirroring the actually-reachable behavior
/// of `khive-pack-kg::handlers::common::optional_string_patch`/
/// `description_patch`, reimplemented here rather than imported (that
/// module has no dependency edge back to `khive-runtime`). Canonical's field
/// type is `Option<Value>` (`UpdateParams.name`/`.description`); serde_json's
/// derived `Deserialize` for `Option<T>` intercepts a literal JSON `null` at
/// the outer `Option` boundary and maps it straight to Rust `None`
/// regardless of the inner type, so canonical's own "clear" arm is
/// unreachable through normal struct deserialization: `update(name=null)` /
/// `update(description=null)` are no-ops, not clears. This module reads raw,
/// un-deserialized JSON, so it must replicate that collapse explicitly: key
/// absent OR JSON `null` -> `None` (leave unchanged, no-op); key present as a
/// string -> `Some(Some(s))` (set); any other JSON type -> a hard error.
fn optional_string_patch(args: &Value, key: &str) -> RuntimeResult<Option<Option<String>>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// Strict string-or-absent-or-null patch for entity `name`. Unlike
/// `optional_str`'s `.as_str()`, this does not silently drop a non-string,
/// non-null value like `name: 123` as absent: it rejects it instead of
/// reporting success for an invalid update. Canonical validates entity
/// `name` via `string_value` on `UpdateParams.name: Option<Value>`: null
/// collapses to absent at the struct-deserialize boundary (see
/// `optional_string_patch` doc above), so the reachable behavior is:
/// absent/null -> unchanged; non-null string -> set; any other JSON type ->
/// hard error. This mirrors that exactly, reading raw JSON instead of a
/// deserialized struct.
fn entity_name_patch(args: &Value) -> RuntimeResult<Option<String>> {
    match obj(args)?.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "name must be a string, got: {other}"
        ))),
    }
}

/// Nullable-JSON-value patch for `properties`: canonical
/// `properties: Option<Value>` on `UpdateParams` collapses a literal JSON
/// `null` to Rust `None` at the struct-deserialize boundary (same collapse
/// as `optional_string_patch` above), so `properties=null` is canonically a
/// no-op (leave existing properties unchanged): not a stored JSON `null`.
/// This module reads raw JSON, so it must replicate that collapse: key
/// absent OR JSON `null` -> `None` (no merge); any other JSON value ->
/// `Some(value)` (merge), with no further shape validation at this layer.
fn optional_properties(args: &Value, key: &str) -> RuntimeResult<Option<Value>> {
    match obj(args)?.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(v.clone())),
    }
}

/// `tags` patch: canonical `tags: Option<Vec<String>>` on `UpdateParams`
/// collapses a literal JSON `null` to Rust `None` at the struct-deserialize
/// boundary (same collapse as above), so `tags=null` is canonically a no-op
/// (leave existing tags unchanged). A non-array, non-null value is still a
/// hard error (mirrors the type failure `UpdateParams` deserialization would
/// itself produce for a malformed `tags`).
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
/// present as a number -> `Some(Some(v))` (set). Range validation lives in
/// curation.rs's `prepare_update_note`, not here.
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
/// `delete_note`'s `record_tok`/`record_ns` convention: not the caller
/// token's namespace, per by-ID namespace-agnosticism) onto `statements`.
///
/// FTS tables (`fts_entities`/`fts_notes`) always exist (created at schema
/// migration time) so their purge is unconditional. `vec_*` tables are
/// created lazily on first vector-store open, so a default runtime can
/// register embedding models before any vector table necessarily exists:
/// a raw unconditional `DELETE FROM vec_*` can hit `no such table` on a
/// fresh DB. Only push the vec purge for tables that actually exist:
/// absence means the record definitionally has no vector row for that
/// model, so skipping is data-parity-correct (the non-atomic path would
/// lazily create the table then delete zero rows: same data outcome,
/// without this read-only prepare pass performing an init side effect).
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
/// lifecycle event after their row mutation: `update_entity` ->
/// `EntityUpdated`, `delete_entity` -> `EntityDeleted`, `delete_note` ->
/// `NoteDeleted`, `update_edge` -> `EdgeUpdated`, `delete_edge` ->
/// `EdgeDeleted`. `update_note` and `link` append no event and must never
/// call this. See `docs/atomic_prepare.md#event_append_statements` for why
/// this is a `PlanStatement` rather than a `PostCommitEffect`.
///
/// Invariant: returned statements are unguarded — appended after the plan's
/// own guarded row statement, so [`apply_plan`]'s stop-on-first-failure
/// contract means they are only reached once that row mutation's guard has
/// already held. Committing the event row atomically with the mutation it
/// describes strengthens canonical's guarantee: the non-atomic handlers write
/// the event in a separate transaction, ordered but not atomic with the row
/// mutation.
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
        // below: callers that need `update(kind=...)` parity must resolve
        // the kind spec themselves (it needs a `VerbRegistry`, unreachable
        // from this crate: see `AtomicUpdateKind`'s doc comment) and call
        // `prepare_update` directly with the resolved value; `kkernel`'s
        // `--atomic` seam does exactly this and bypasses this dispatch arm.
        // A caller reaching `prepare_op("update", ...)` without going
        // through that seam gets kind-unchecked behavior.
        "update" => prepare_update(runtime, token, args, None).await,
        // `expected_kind: None` here — callers that need `delete(kind=...)`
        // parity must resolve the kind spec themselves (it needs a
        // `VerbRegistry`, unreachable from this crate: see
        // `AtomicDeleteKind`'s doc comment) and call `prepare_delete`
        // directly with the resolved value; `kkernel`'s `--atomic` seam does
        // exactly this and bypasses this dispatch arm. A caller reaching
        // `prepare_op("delete", ...)` without going through that seam gets
        // kind-unchecked behavior.
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
// create (AddEntity / AddNote)
// ---------------------------------------------------------------------------

/// Build the prepared plan for an `AddEntity` proposal change. The entity
/// row and FTS document are committed together; vector indexing is deferred
/// until after commit because embedding may suspend. `kind` must already be
/// canonicalized by the caller because pack-aware resolution requires a
/// `VerbRegistry`.
pub async fn prepare_add_entity(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let kind = require_str(args, "kind")?;
    let name = require_str(args, "name")?;
    runtime.validate_entity_kind(kind)?;
    if name.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "name must not be empty".to_string(),
        ));
    }

    let description = optional_create_string(args, "description")?;
    let properties = optional_properties(args, "properties")?;
    let tags = optional_tags(args)?.unwrap_or_default();

    crate::secret_gate::check(name)?;
    if let Some(ref d) = description {
        crate::secret_gate::check(d)?;
    }
    if let Some(ref p) = properties {
        crate::secret_gate::check_json(p)?;
    }
    crate::secret_gate::check_tags(&tags)?;

    let ns = token.namespace().as_str();
    let mut entity = khive_storage::Entity::new(ns, kind, name);
    if let Some(d) = description {
        entity = entity.with_description(d);
    }
    if let Some(p) = properties {
        entity = entity.with_properties(p);
    }
    if !tags.is_empty() {
        entity = entity.with_tags(tags);
    }

    let statements = vec![
        PlanStatement {
            statement: entity_upsert_statement(&entity),
            guard: Some(AffectedRowGuard::exactly(1)),
        },
        PlanStatement {
            statement: insert_document_statement("fts_entities", &entity_fts_document(&entity)),
            guard: None,
        },
    ];

    Ok(AtomicOpPlan::AddEntity(AddEntityPlan {
        entity_id: entity.id,
        statements,
        post_commit: PostCommitEffect::ReindexEntity {
            entity_id: entity.id,
        },
    }))
}

/// Build the prepared plan for an `AddNote` proposal change. Mirrors
/// [`prepare_add_entity`]'s shape and the same
/// `kind`-already-canonicalized split. `annotates` is out of scope: the
/// proposal `NoteDraft` this backs carries no annotates targets, unlike
/// `KhiveRuntime::create_note`'s general-purpose signature.
pub async fn prepare_add_note(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let kind = require_str(args, "kind")?;
    let content = require_str(args, "content")?;
    runtime.validate_note_kind(kind)?;

    let name = optional_create_string(args, "name")?;
    let properties = optional_properties(args, "properties")?;

    crate::secret_gate::check(content)?;
    if let Some(ref n) = name {
        crate::secret_gate::check(n)?;
    }
    if let Some(ref p) = properties {
        crate::secret_gate::check_json(p)?;
    }

    let ns = token.namespace().as_str();
    let mut note = khive_storage::note::Note::new(ns, kind, content);
    if let Some(n) = name {
        note = note.with_name(n);
    }
    if let Some(p) = properties {
        note = note.with_properties(p);
    }

    let statements = vec![
        PlanStatement {
            statement: note_upsert_statement(&note),
            guard: Some(AffectedRowGuard::exactly(1)),
        },
        PlanStatement {
            statement: insert_document_statement("fts_notes", &note_fts_document(&note)),
            guard: None,
        },
    ];

    Ok(AtomicOpPlan::AddNote(AddNotePlan {
        note_id: note.id,
        statements,
        post_commit: PostCommitEffect::ReindexNote { note_id: note.id },
    }))
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// Mirrors `khive-pack-kg::handlers::update::reject_inapplicable_fields`: a
/// hard `InvalidInput` when a caller passes a field that does not apply to
/// the resolved substrate (e.g. `salience` on an entity, or
/// `description`/`tags` on a note). That function has no dependency edge
/// back to `khive-runtime`, so its exact field-applicability check list and
/// error message shape are reimplemented here rather than imported: same
/// pattern as `optional_string_patch` above. Presence is checked directly on
/// the raw args object (this module has no `UpdateParams` struct); a JSON
/// `null` value is treated as absent, matching `Option<T>` deserialization
/// semantics.
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
        // `update` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`, so
        // this arm must reject entity/note-only fields (e.g. `name`) on an
        // edge update rather than silently skip the guard, mirroring
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
/// `resolve_kind_spec` at the kkernel `--atomic` seam: the same pattern
/// [`AtomicDeleteKind`] uses. Without this check, `update(kind="document",
/// id=<concept>)` would be canonically `NotFound` but the atomic path would
/// ignore the explicit kind and mutate the resolved entity anyway.
/// `khive-runtime` must not depend on `khive-pack-kg`, so this is a plain
/// substrate-level shape rather than `khive_pack_kg::handlers::KindSpec`
/// itself: the kkernel seam does the pack-aware resolution and passes down
/// only what `prepare_update` needs to enforce the mismatch check.
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

    // Mirrors update.rs's entity_kind immutability guard: entity_kind is a
    // legacy top-level field, independent of the `kind` substrate
    // discriminator handled elsewhere.
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

            prepare_update_entity_plan(
                runtime,
                token,
                id,
                crate::curation::EntityPatch {
                    name,
                    description,
                    properties,
                    tags,
                },
            )
            .await
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
            // same function canonical `update_note` calls, including the
            // salience/decay_factor range validation. `optional_f64_patch`
            // below preserves tri-state patch semantics (key absent =
            // untouched, key null = clear, key present = set) when
            // constructing the `NotePatch`.
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
        // entity/note-then-edge fallback order. `update` admits
        // `kind="edge"`, so this arm must be able to build a plan for one.
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

/// Build an entity update plan from a typed patch. Proposal changesets use
/// this entry point so their explicit `description: null` clear operation is
/// preserved instead of being collapsed by raw verb deserialization.
pub async fn prepare_update_entity_plan(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    patch: crate::curation::EntityPatch,
) -> RuntimeResult<AtomicOpPlan> {
    let (entity, text_changed, changed_fields) =
        runtime.prepare_update_entity(token, id, patch).await?;
    let mut statements = vec![PlanStatement {
        statement: entity_upsert_statement(&entity),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
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

/// Edge branch of `prepare_update`. Mirrors `KhiveRuntime::update_edge`'s
/// patch semantics: `relation`/`weight`/`properties` are the only applicable
/// fields, a changed `relation` is endpoint-validated first, `weight` is
/// range-checked, and `properties` REPLACES `metadata` wholesale (no merge).
/// See `docs/atomic_prepare.md#prepare_update_edge` for the DML-shape parity
/// detail with `update_edge`.
///
/// Invariant (symmetric relations `competes_with`/`composed_with`): this
/// function must never branch on a prepare-time conflict probe — a different
/// op in the same atomic unit could change the conflict landscape between
/// probe and commit, making any such branch stale by construction. It always
/// emits BOTH statements from [`edge_symmetric_delete_if_conflict_statement`]
/// and [`edge_symmetric_refresh_or_update_inplace_statement`], each carrying
/// its own commit-time `WHERE`/`CASE WHEN` predicate that re-evaluates the
/// conflict condition fresh inside the transaction. This function reads no
/// state to guess a surviving id; the plan instead carries `edge_natural_key`
/// so a post-commit caller derives the actual surviving id from the
/// committed row, never from a value computed before the rest of this atomic
/// unit has even run.
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
        // The write for a symmetric relation never branches on a
        // prepare-time probe result: it always carries both self-guarding,
        // commit-time-predicate statements (see their doc comment in
        // khive-db's graph.rs for the full rationale). This avoids the
        // staleness window a prepare-time probe would expose: an earlier op
        // in the same atomic unit could change the conflict landscape before
        // commit. Canonical's own probe-then-branch
        // `update_edge_symmetric_dml` has no such exposure (single
        // transaction, no interleaving) and is unaffected.
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
    // event append, keyed on the original `edge_id` the caller supplied:
    // canonical does the same (the event target is `edge_id`, not the
    // post-absorption surviving id).
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
/// `resolve_kind_spec` at the kkernel `--atomic` seam. `khive-runtime` must
/// not depend on `khive-pack-kg` (packs depend on the runtime, not the other
/// way around), so this is a plain substrate-level shape rather than
/// `khive_pack_kg::handlers::KindSpec` itself: the kkernel seam does the
/// pack-aware `resolve_kind_spec` resolution (which needs a `VerbRegistry`,
/// unreachable from this crate) and passes down only what `prepare_delete`
/// needs to enforce the mismatch check.
///
/// `delete` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`, hence the
/// `Edge` variant. `Event`/`Proposal` remain rejected at the kkernel seam
/// (not v1-admissible for atomic delete at all).
pub enum AtomicDeleteKind {
    Entity { specific: Option<String> },
    Note { specific: Option<String> },
    Edge,
}

/// `expected_kind`: `None` when the caller omitted `kind` (no check, parity
/// with canonical's own optional discriminator); `Some(_)` enforces an
/// exact-parity mismatch check against the resolved record's actual
/// substrate/specific kind, mirroring `handle_delete`'s
/// `entity.kind != *expected` / `note.kind != *expected` checks.
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

    // `delete(id, hard=true)` is the public purge route after a prior soft
    // delete, so it must resolve including already-tombstoned rows (a
    // live-only resolve would never find one). Soft delete keeps the
    // live-only resolve: a soft delete of an already-tombstoned row is a
    // no-op, matching non-atomic behavior.
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
            // FTS + vector index purge, matching operations.rs
            // `delete_entity`: both soft and hard delete clean indexes (a
            // hard delete of an already-tombstoned record must still purge
            // them); only hard additionally cascades edges above.
            push_index_purge_statements(
                runtime,
                &mut statements,
                "fts_entities",
                &namespace,
                id,
                "atomic-delete-entity",
            )
            .await?;
            // operations.rs's `delete_entity` appends an `EntityDeleted`
            // event after a successful row delete, on both soft and hard
            // delete. `apply_plan` never reaches this statement unless the
            // guarded row statement above affected a row, so no extra `if`
            // is needed here.
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
                post_commit: PostCommitEffect::None,
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
            // FTS + vector index purge, matching operations.rs
            // `delete_note`: both soft and hard delete clean indexes (a hard
            // delete of an already-tombstoned record must still purge
            // them); only hard additionally cascades edges above.
            push_index_purge_statements(
                runtime,
                &mut statements,
                "fts_notes",
                &namespace,
                id,
                "atomic-delete-note",
            )
            .await?;
            // operations.rs's `delete_note` appends a `NoteDeleted` event
            // after a successful row delete, on both soft and hard delete:
            // same reasoning as the entity branch above.
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
                // A committed atomic note delete must fire the same
                // pack-installed note-mutation hook `operations.rs::
                // delete_note` fires, so a warm ANN cache sees the deletion
                // even when the mutation went through the atomic-plan path.
                post_commit: PostCommitEffect::NoteDeleted {
                    note_id: id,
                    kind: note.kind.clone(),
                },
            }))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "delete target {id} must be an entity, note, or edge"
        ))),
        // `Resolved` has no `Edge` variant (same reasoning as
        // `prepare_update`'s fallback above) — probe the graph store
        // directly.
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

/// Edge branch of `prepare_delete`. Mirrors
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
        post_commit: PostCommitEffect::None,
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
    // the key when metadata doesn't already carry one. Calls the same
    // `khive_runtime::merge_entry_metadata` `khive-pack-kg`'s canonical
    // `handle_link` calls, so both sides depend on one function instead of
    // each maintaining their own copy.
    let mut metadata = crate::merge_entry_metadata(
        metadata,
        optional_str(args, "dependency_kind").map(String::from),
    )?;

    validate_edge_weight(weight)?;
    runtime
        .validate_edge_relation_endpoints(token, source_id, target_id, relation)
        .await?;

    let (canon_source, canon_target) = canonical_edge_endpoints(relation, source_id, target_id);

    // Endpoint-kind `dependency_kind` inference for `depends_on` edges,
    // matching operations.rs `link()`: only applies when both endpoints
    // resolve as entities and the key is still absent after the
    // top-level-param merge above. Runs against the canonical endpoints,
    // mirroring `KhiveRuntime::link`'s own ordering (canonicalize, then
    // infer).
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

    // The guarded `INSERT ... SELECT ... WHERE EXISTS(...)` shape is
    // load-bearing (see `LinkPlan`'s own doc comment): it re-probes both
    // endpoints inside the transaction, closing the intra-batch hazard
    // where an earlier op in the same atomic unit, e.g. `delete(X, hard)`,
    // could invalidate this op's prepare-time endpoint validation before
    // commit. The conflict-arm SET list shares the same
    // `EDGE_NATURAL_KEY_CONFLICT_SET` text `edge_upsert_statement`
    // (canonical `link`'s builder) uses, so the two cannot silently diverge
    // (a prior bug: this atomic literal never set
    // `target_backend = excluded.target_backend`, so a re-link of an edge
    // carrying a cross-backend `target_backend` stamp behaved differently
    // under `--atomic`).
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
// merge (entity-only)
// ---------------------------------------------------------------------------

// Full atomic-merge parity (field folding, survivor FTS/vector reindex,
// loser index purge, merge provenance, same-kind rejection) is deferred:
// atomic `merge` is rejected entirely at the pre-runtime admissibility
// guard (`khive_types::pack::ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`, alongside
// `propose`/`review`/`withdraw`). This function still produces a plan
// (kept for the existing direct-prepare test coverage below and as
// defense in depth), but the CLI's `--atomic` surface never reaches it,
// since `check_atomic_admissible` rejects `merge` before any runtime is
// built.
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
/// outside any transaction. Re-fetches each target's now-committed row and
/// reuses the existing `reindex_entity`/`reindex_note` (FTS + embedding,
/// same as the non-atomic path) for exact parity.
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
                    // This handler calls `reindex_note` directly, bypassing
                    // `update_note()` and the note-mutation hook it fires
                    // after its own reindex (see `curation.rs`). Fire it
                    // here so any in-process consumer (e.g.
                    // khive-pack-memory's warm ANN cache) sees a bumped
                    // generation after a committed atomic note update,
                    // matching the non-atomic path.
                    runtime.fire_note_mutation_hook(&note.kind, note.id).await;
                }
            }
            PostCommitEffect::NoteDeleted { note_id, kind } => {
                // Unlike `operations.rs`'s `delete_note`, which fires
                // `fire_note_mutation_hook` directly (with the already-known
                // kind, no refetch) after a successful row delete, an atomic
                // note delete needs this post-commit pass to reach it. The
                // note row is gone (hard delete) or tombstoned (soft
                // delete) by the time this runs, so it mirrors
                // `delete_note`'s direct-fire shape rather than
                // `ReindexNote`'s refetch-then-fire shape.
                runtime.fire_note_mutation_hook(&kind, note_id).await;
            }
            PostCommitEffect::GtdAudit { .. } => {
                // Applied by the `kkernel` caller's own post-commit pass,
                // not here: `khive-pack-gtd` (owner of
                // `ensure_audit_schema`/`write_audit_record`) depends on
                // `khive-runtime`, not the other way around, so this crate
                // cannot act on the effect itself. See
                // `PostCommitEffect::GtdAudit`'s doc comment.
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

    /// Atomic `update` must reject a field that does not apply to the
    /// resolved substrate: parity with
    /// `khive-pack-kg::handlers::update::reject_inapplicable_fields`.
    /// Without this check, atomic prepare would silently ignore `salience`
    /// on an entity: it would set every entity field to its current value,
    /// bump `updated_at`, satisfy the `exactly(1)` guard, and commit: a
    /// spurious no-op reported as success.
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

    /// Symmetric note-substrate case: `description` and `tags` are
    /// entity-only fields; passing either for a note must be rejected the
    /// same way update.rs rejects them.
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

    /// Updating a note's content inside an atomic unit must, after commit,
    /// leave the note recallable via FTS under its new content and its
    /// vector row refreshed: parity with the non-atomic
    /// `update_note` -> `reindex_note` path.
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

    /// The atomic-plan path must fire the pack-installed note-mutation hook
    /// for both an atomic note UPDATE (`PostCommitEffect::ReindexNote`'s
    /// handler fires it after its own reindex, mirroring `update_note()`
    /// on the non-atomic path) and an atomic note DELETE (`DeletePlan`
    /// carries a `PostCommitEffect::NoteDeleted` that
    /// `apply_post_commit_effects` dispatches directly, mirroring
    /// `operations.rs::delete_note`'s direct-fire, no-refetch shape: the
    /// row may already be gone by the time this runs, for a hard delete).
    /// A minimal counting hook proves both fire; no `khive-pack-memory`
    /// dependency is needed at this layer, since the hook itself is
    /// generic.
    #[tokio::test]
    async fn atomic_note_update_and_delete_post_commit_fire_the_note_mutation_hook() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let fired: std::sync::Arc<std::sync::Mutex<Vec<(String, uuid::Uuid)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let fired_for_hook = fired.clone();
        runtime.install_note_mutation_hook(std::sync::Arc::new(
            move |kind: String, id: uuid::Uuid| {
                let fired = fired_for_hook.clone();
                Box::pin(async move {
                    fired.lock().expect("lock").push((kind, id));
                })
            },
        ));

        // Update path.
        let mut note = khive_storage::note::Note::new("local", "observation", "hook-update-target");
        note.name = Some("hook-update-target".to_string());
        let update_note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed update-target note");

        let plan = prepare_update(
            &runtime,
            &token,
            &json!({"id": update_note_id.to_string(), "content": "hook-update-target, revised"}),
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
        apply_post_commit_effects(&runtime, &token, post_commit)
            .await
            .expect("apply post-commit effects (update)");

        // Delete path (soft delete: the row still exists, but the hook
        // fires directly from the captured kind rather than refetching).
        let mut del_note =
            khive_storage::note::Note::new("local", "observation", "hook-delete-target");
        del_note.name = Some("hook-delete-target".to_string());
        let delete_note_id = del_note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(del_note)
            .await
            .expect("seed delete-target note");

        let plan = prepare_delete(
            &runtime,
            &token,
            &json!({"id": delete_note_id.to_string(), "hard": false}),
            None,
        )
        .await
        .expect("prepare delete");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        let post_commit = match outcome {
            crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected Committed, got {other:?}"),
        };
        assert_eq!(
            post_commit,
            vec![PostCommitEffect::NoteDeleted {
                note_id: delete_note_id,
                kind: "observation".to_string(),
            }],
            "a committed note delete must schedule exactly one NoteDeleted post-commit effect"
        );
        apply_post_commit_effects(&runtime, &token, post_commit)
            .await
            .expect("apply post-commit effects (delete)");

        let seen = fired.lock().expect("lock").clone();
        assert!(
            seen.contains(&("observation".to_string(), update_note_id)),
            "the note-mutation hook must fire for the atomic UPDATE path: {seen:?}"
        );
        assert!(
            seen.contains(&("observation".to_string(), delete_note_id)),
            "the note-mutation hook must fire for the atomic DELETE path: {seen:?}"
        );
    }

    /// Atomic delete must purge the note's FTS row and vector row for both
    /// soft and hard delete: parity with `KhiveRuntime::delete_note`'s
    /// index-cleanup contract.
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

    /// Atomic delete must purge the entity's FTS row and vector row for
    /// both soft and hard delete: parity with
    /// `KhiveRuntime::delete_entity`'s index-cleanup contract.
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

    /// Atomic link must persist an explicit top-level `dependency_kind`
    /// param into edge metadata, and must infer one for `depends_on` edges
    /// when absent: parity with
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

    /// Atomic `link` must be an upsert, exactly like canonical `link` ->
    /// `upsert_edge`'s natural-key `ON CONFLICT` arm: re-linking an
    /// already-linked triple must succeed and update weight/metadata, not
    /// hit the `UNIQUE(namespace, source_id, target_id, relation)`
    /// constraint and roll back the whole atomic unit.
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

    /// Atomic `link` of a soft-deleted triple must resurrect it
    /// (`deleted_at = NULL`), matching `upsert_edge`'s natural-key
    /// `ON CONFLICT ... DO UPDATE SET deleted_at = NULL`.
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

    /// Atomic delete of an entity and a note must succeed even when the
    /// registered embedding model's `vec_*` table has never been lazily
    /// created (a fresh DB registers models before any vector store is
    /// opened): the raw purge DML must skip tables that don't exist
    /// rather than hit `no such table` and roll back the whole atomic
    /// unit. FTS purge still fires (those tables always exist) and the
    /// delete itself is a clean commit.
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

    /// Atomic hard delete must be able to purge a record that was already
    /// soft-deleted: parity with `delete(id, hard=true)` being the public
    /// purge route after a prior soft delete (the non-atomic hard path
    /// resolves including deleted rows and its DML carries no `deleted_at`
    /// predicate).
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
    // event-store append parity
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

    /// Atomic `update(id=<entity>, name=...)` must append an
    /// `EntityUpdated` event, matching `curation::update_entity`: the
    /// event is appended unconditionally after a successful row update,
    /// not only on the reindex-triggering subset.
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

    /// Atomic soft and hard delete of an entity must each append an
    /// `EntityDeleted` event, matching `operations::delete_entity`, which
    /// fires on both delete modes.
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

    /// Atomic soft and hard delete of a note must each append a
    /// `NoteDeleted` event, matching `operations::delete_note`, which
    /// fires on both delete modes.
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

    /// `update` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`; this
    /// asserts `prepare_update` actually builds a plan for one, a
    /// non-symmetric relation (`extends`) exercises the
    /// `edge_upsert_statement` reuse branch, and that the committed row +
    /// `EdgeUpdated` event match canonical `update_edge`'s shape (weight
    /// persisted, relation unchanged, exactly one event).
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

    /// The symmetric-relation conflict-absorption branch of
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
        // The plan does not compute a prepare-time advisory surviving id
        // (`target_id` is just the requested id): it carries
        // `edge_natural_key` so a post-commit caller can derive the real
        // surviving id itself. Assert the plan carries the right natural
        // key to look up; the actual surviving row's identity is verified
        // against the DB after commit, below.
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

    /// The same-unit race: `[delete(X), update(X -> competes_with)]` where
    /// an already-existing canonical row sits at the post-update natural
    /// key. Both ops' async prepare passes run before either commits, so at
    /// prepare time `X` still exists and both plans build. At commit time
    /// `delete(X)` removes it first; `update(X -> competes_with)`'s own
    /// commit-time statements must then fail loud (its target no longer
    /// exists) rather than silently absorbing into the pre-existing
    /// canonical row it never causally touched. The whole atomic unit must
    /// roll back — parity with canonical `update_edge`'s `NotFound` for a
    /// missing edge, expressed here as the unit-level abort for any op
    /// whose commit-time guard fails.
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

    /// `update` rejects an entity/note-only field (`name`) on an edge
    /// target, mirroring
    /// `khive-pack-kg::handlers::update::reject_inapplicable_fields`'s
    /// `KindSpec::Edge` arm.
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

    /// `delete` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`; this
    /// asserts `prepare_delete` actually builds a plan for one on both soft
    /// and hard delete, matching `operations::delete_edge`'s row-mode DML
    /// and unconditional `EdgeDeleted` event.
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

    /// Parity boundary: atomic `update` of a note must append no event:
    /// canonical `update_note` never calls `append_event` (unlike
    /// `update_entity`, which always does).
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

    /// Parity boundary: atomic `link` must append no event: canonical
    /// `link` never calls `append_event`.
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

    // ------------------------------------------------------------------
    // AddEntity and AddNote plans alongside link
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn prepare_add_entity_rejects_whitespace_only_name() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let err = prepare_add_entity(&runtime, &token, &json!({"kind": "concept", "name": "   "}))
            .await
            .expect_err("whitespace-only entity name must fail prepare");

        assert!(matches!(
            err,
            RuntimeError::InvalidInput(message) if message.contains("name must not be empty")
        ));
    }

    #[tokio::test]
    async fn prepare_add_entity_rejects_non_string_description() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let err = prepare_add_entity(
            &runtime,
            &token,
            &json!({"kind": "concept", "name": "Valid", "description": 42}),
        )
        .await
        .expect_err("non-string entity description must fail prepare");

        assert!(matches!(
            err,
            RuntimeError::InvalidInput(message)
                if message.contains("description must be a string or null")
        ));
    }

    #[tokio::test]
    async fn prepare_add_note_rejects_non_string_name() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        let err = prepare_add_note(
            &runtime,
            &token,
            &json!({"kind": "observation", "content": "Valid", "name": 42}),
        )
        .await
        .expect_err("non-string note name must fail prepare");

        assert!(matches!(
            err,
            RuntimeError::InvalidInput(message) if message.contains("name must be a string or null")
        ));
    }

    #[tokio::test]
    async fn atomic_add_entity_link_add_note_plan_commits_entity_edge_note_and_fts_together() {
        let runtime = scratch_runtime();
        runtime.register_embedder(StubProvider);
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", "ProposalPlanLinkA");
        let b = khive_storage::Entity::new("local", "concept", "ProposalPlanLinkB");
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");

        let add_entity_plan = prepare_add_entity(
            &runtime,
            &token,
            &json!({"kind": "concept", "name": "ProposalPlanNewEntity", "description": "created atomically"}),
        )
        .await
        .expect("prepare add_entity");
        let link_plan = prepare_link(
            &runtime,
            &token,
            &json!({"source_id": a_id.to_string(), "target_id": b_id.to_string(), "relation": "extends"}),
        )
        .await
        .expect("prepare link");
        let add_note_plan = prepare_add_note(
            &runtime,
            &token,
            &json!({"kind": "observation", "content": "created atomically alongside the entity"}),
        )
        .await
        .expect("prepare add_note");

        let entity_id = match &add_entity_plan {
            AtomicOpPlan::AddEntity(p) => p.entity_id,
            other => panic!("expected an AddEntity plan, got {other:?}"),
        };
        let note_id = match &add_note_plan {
            AtomicOpPlan::AddNote(p) => p.note_id,
            other => panic!("expected an AddNote plan, got {other:?}"),
        };

        let outcome = crate::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![add_entity_plan, link_plan, add_note_plan],
        )
        .await
        .expect("seam call ok");
        let post_commit = match outcome {
            crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected the whole unit to commit: {other:?}"),
        };
        let entity = runtime
            .entities(&token)
            .expect("entities store")
            .get_entity(entity_id)
            .await
            .expect("get_entity")
            .expect("entity must exist after commit");
        assert_eq!(entity.name, "ProposalPlanNewEntity");
        assert!(
            runtime
                .text(&token)
                .expect("text store")
                .get_document("local", entity_id)
                .await
                .expect("get_document")
                .is_some(),
            "entity's FTS document must exist after commit"
        );

        let (edge_count, _, _, edge_deleted_at) =
            probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
        assert_eq!(
            edge_count, 1,
            "the edge must be committed alongside the entity/note"
        );
        assert!(edge_deleted_at.is_none());

        let note = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(note_id)
            .await
            .expect("get_note")
            .expect("note must exist after commit");
        assert_eq!(note.content, "created atomically alongside the entity");
        assert!(
            runtime
                .text_for_notes(&token)
                .expect("text store")
                .get_document("local", note_id)
                .await
                .expect("get_document")
                .is_some(),
            "note's FTS document must exist after commit"
        );

        apply_post_commit_effects(&runtime, &token, post_commit)
            .await
            .expect("apply post-commit effects");

        let vec_store = runtime
            .vectors_for_model(&token, STUB_MODEL)
            .expect("vec store");
        assert_eq!(
            vec_store.count().await.expect("count after"),
            2,
            "post-commit reindex must have embedded both the new entity and the new note"
        );
    }

    #[tokio::test]
    async fn atomic_add_entity_and_add_note_roll_back_on_later_link_failure_leaving_zero_trace() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", "ProposalPlanRollbackA");
        let x = khive_storage::Entity::new("local", "concept", "ProposalPlanRollbackX");
        let (a_id, x_id) = (a.id, x.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(x.clone()).await.expect("seed x");

        let add_entity_plan = prepare_add_entity(
            &runtime,
            &token,
            &json!({"kind": "concept", "name": "ProposalPlanRollbackNewEntity"}),
        )
        .await
        .expect("prepare add_entity");
        let add_note_plan = prepare_add_note(
            &runtime,
            &token,
            &json!({"kind": "observation", "content": "must not survive the rollback"}),
        )
        .await
        .expect("prepare add_note");
        let delete_plan = prepare_delete(
            &runtime,
            &token,
            &json!({"id": x_id.to_string(), "hard": true}),
            None,
        )
        .await
        .expect("prepare delete x");
        // Prepare sees x before the transaction; the guarded link must detect
        // that the preceding hard delete removed it inside the transaction.
        let link_plan = prepare_link(
            &runtime,
            &token,
            &json!({"source_id": a_id.to_string(), "target_id": x_id.to_string(), "relation": "extends"}),
        )
        .await
        .expect("prepare link (endpoint still exists at prepare time)");

        let entity_id = match &add_entity_plan {
            AtomicOpPlan::AddEntity(p) => p.entity_id,
            other => panic!("expected an AddEntity plan, got {other:?}"),
        };
        let note_id = match &add_note_plan {
            AtomicOpPlan::AddNote(p) => p.note_id,
            other => panic!("expected an AddNote plan, got {other:?}"),
        };

        let outcome = crate::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![add_entity_plan, add_note_plan, delete_plan, link_plan],
        )
        .await
        .expect("the seam call itself must not error; the unit rolls back cleanly");
        match outcome {
            crate::atomic_runner::AtomicRunOutcome::RolledBack {
                failed_op_index, ..
            } => {
                assert_eq!(
                    failed_op_index, 3,
                    "the trailing link (index 3) must be the op whose guard fails"
                );
            }
            other => panic!("expected the whole unit to roll back, got {other:?}"),
        }

        assert!(
            runtime
                .get_entity_including_deleted(&token, entity_id)
                .await
                .expect("get_entity_including_deleted")
                .is_none(),
            "the new entity must leave zero trace after rollback"
        );
        assert!(
            runtime
                .text(&token)
                .expect("text store")
                .get_document("local", entity_id)
                .await
                .expect("get_document")
                .is_none(),
            "the new entity's FTS document must leave zero trace after rollback"
        );
        assert!(
            runtime
                .get_note_including_deleted(&token, note_id)
                .await
                .expect("get_note_including_deleted")
                .is_none(),
            "the new note must leave zero trace after rollback"
        );
        assert!(
            runtime
                .text_for_notes(&token)
                .expect("text store")
                .get_document("local", note_id)
                .await
                .expect("get_document")
                .is_none(),
            "the new note's FTS document must leave zero trace after rollback"
        );

        let x_after = runtime
            .get_entity_including_deleted(&token, x_id)
            .await
            .expect("get_entity_including_deleted")
            .expect("x must still be present because its delete rolled back too");
        assert!(
            x_after.deleted_at.is_none(),
            "x's delete must have rolled back along with the failed link"
        );

        let (edge_count, _, _, _) =
            probe_edge_natural_key(&runtime, "local", a_id, x_id, "extends").await;
        assert_eq!(edge_count, 0, "no edge may have been committed");
    }
}
