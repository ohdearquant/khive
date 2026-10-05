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
//! `docs/api/atomic_prepare.md#scope-what-is-excluded-and-why` for why each of these is excluded
//! and what would be required to admit them.

use serde_json::Value;
use uuid::Uuid;

use khive_storage::types::SqlValue;
use khive_storage::{AttachmentSubstrate, EdgeRelation, EdgeUpsertDisposition, SqlStatement};
use khive_types::pack::pack_registry_tag;
use khive_types::{EventKind, SubstrateKind};

use crate::atomic_plan::{
    AddEntityPlan, AddNotePlan, AffectedRowGuard, DeletePlan, EdgeNaturalKey, LinkPlan, MergePlan,
    PlanStatement, PostCommitEffect, UpdatePlan,
};
use crate::atomic_runner::AtomicOpPlan;
use crate::atomic_runner::CommittedPostCommitEffects;
use crate::curation::{entity_fts_document, note_fts_document};
use crate::error::{RuntimeError, RuntimeResult};
use crate::operations::{
    canonical_edge_endpoint_kinds, canonical_edge_endpoints, merge_dependency_kind,
    validate_edge_metadata, validate_edge_weight, Resolved,
};
use crate::runtime::{KhiveRuntime, NamespaceToken};

use khive_db::stores::attachment::delete_record_attachments_statement;
use khive_db::stores::entity::{
    entity_hard_delete_statement, entity_replace_if_unchanged_statement,
    entity_soft_delete_statement, entity_upsert_statement,
};
use khive_db::stores::event::event_insert_statements;
use khive_db::stores::event::hard_delete_lineage_warning_statements;
use khive_db::stores::graph::{
    edge_hard_delete_statement, edge_insert_new_guarded_by_endpoints_statement,
    edge_link_replace_if_unchanged_and_endpoints_exist_statement,
    edge_replace_if_unchanged_statement, edge_soft_delete_statement,
    edge_symmetric_absorb_or_update_inplace_statement, edge_symmetric_delete_if_conflict_statement,
    purge_incident_edges_statement,
};
use khive_db::stores::note::{
    note_hard_delete_statement, note_soft_delete_statement, note_upsert_statement,
};
use khive_db::stores::text::{delete_document_statements, insert_document_statements};

mod add_update;
pub use add_update::{
    prepare_add_entity, prepare_add_note, prepare_op, prepare_update, prepare_update_entity_plan,
    prepare_update_from_note_snapshot, validate_note_update_expected_kind, AtomicUpdateKind,
};
use add_update::{
    prepare_update_entity_plan_with_version_and_type, reject_inapplicable_update_fields,
};

mod note_reindex_effect;

// ---------------------------------------------------------------------------
// arg extraction helpers
// ---------------------------------------------------------------------------

fn obj(args: &Value) -> RuntimeResult<&serde_json::Map<String, Value>> {
    args.as_object()
        .ok_or_else(|| RuntimeError::InvalidInput("op args must be a JSON object".into()))
}

fn refuse_pack_registry_tags(tags: &[String], verb: &str) -> RuntimeResult<()> {
    let Some(tag) = tags.iter().find_map(|tag| pack_registry_tag(tag)) else {
        return Ok(());
    };
    Err(RuntimeError::InvalidInput(format!(
        "{verb} refuses registry tag {tag:?}: registry rows are written only by the owning pack"
    )))
}

fn require_str<'a>(args: &'a Value, key: &str) -> RuntimeResult<&'a str> {
    obj(args)?
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| RuntimeError::InvalidInput(format!("missing required field {key:?}")))
}

/// Parse `key` as a bare UUID — never a short hex prefix.
///
/// A short prefix is a *resolution*: an unfiltered search that applies no
/// namespace predicate, so it can match nothing, match exactly one record
/// across every namespace, or match ambiguously. That search already
/// happened upstream, at the `kkernel` CLI boundary that has namespace
/// context and calls `resolve_uuid_unfiltered` before handing args down to
/// this module
/// (`crates/kkernel/src/atomic_apply.rs::resolve_kg_ids_in_args`). By the
/// time an id reaches this plan-preparation stage it must already name one
/// specific, already-identified record — which is exactly what a full UUID
/// demonstrates and a prefix does not.
fn require_uuid(args: &Value, key: &str) -> RuntimeResult<Uuid> {
    let raw = require_str(args, key)?;
    Uuid::parse_str(raw).map_err(|_| {
        RuntimeError::InvalidInput(format!(
            "{key} must be a full UUID; got {raw:?}. This atomic-plan stage consumes an \
             already-resolved record and performs no namespace-scoped search of its own, so a \
             short hex prefix cannot be resolved here — resolve it to a full UUID first (e.g. \
             via `get`) and pass that."
        ))
    })
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

/// ADR-014 tri-state patch for entity `entity_type`, read from raw JSON to
/// mirror `UpdateParams.entity_type`'s `tri_string` deserializer (the
/// kkernel `--atomic` seam deserializes through that struct first, so the
/// two surfaces cannot diverge): key absent -> `None` (unchanged), key
/// present as `null` -> `Some(None)` (explicit clear), key present as a
/// string -> `Some(Some(s))` (set); any other JSON type -> a hard error.
fn optional_entity_type_patch(args: &Value, key: &str) -> RuntimeResult<Option<Option<String>>> {
    match obj(args)?.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(value)) => Ok(Some(Some(value.clone()))),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string or null, got: {other}"
        ))),
    }
}

/// Nullable-string patch semantics shared by note updates and mirroring the
/// actually-reachable entity description behavior of
/// `khive-pack-kg::handlers::common::description_patch`. Canonical's field
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
/// curation.rs's `prepare_update_note_from_snapshot`, not here.
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
/// against one vector table for a single subject, scoped by namespace.
///
/// Vector tables carry a real index on `(subject_id, namespace)`
/// ([`khive_db::stores::vectors`]) — this row-scan predicate is not the FTS
/// full-table-scan class this module's `purge_fts_document_statements`
/// exists to avoid, so it is left as a direct `namespace = ? AND subject_id =
/// ?` predicate.
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

fn purge_vector_provenance_statement(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    label: &str,
) -> PlanStatement {
    let model_key = table
        .strip_prefix("vec_")
        .expect("runtime vector tables use the vec_ prefix");
    PlanStatement {
        statement: SqlStatement {
            sql: "DELETE FROM vector_provenance \
                  WHERE model_key = ?1 AND subject_id = ?2 AND namespace = ?3"
                .to_string(),
            params: vec![
                SqlValue::Text(model_key.to_string()),
                SqlValue::Text(subject_id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

/// The FTS-document half of an index purge: `fts_table`'s row for `subject_id`
/// (looked up via `khive_db::stores::text::rowid_map_table`, not a
/// `namespace`/`subject_id` scan — those columns are `UNINDEXED` in every
/// FTS5 DDL) plus that row's own entry in the sidecar map. Order-sensitive:
/// index 0 must run before index 1 — see `delete_document_statements`'s
/// adjacency contract.
fn purge_fts_document_statements(
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
) -> [PlanStatement; 2] {
    let [mut fts_stmt, mut map_stmt] = delete_document_statements(fts_table, namespace, subject_id);
    fts_stmt.label = Some(label_prefix.to_string());
    map_stmt.label = Some(format!("{label_prefix}-map"));
    [
        PlanStatement {
            statement: fts_stmt,
            guard: None,
        },
        PlanStatement {
            statement: map_stmt,
            guard: None,
        },
    ]
}

fn log_vector_row_delete_statement(
    table: &str,
    namespace: &str,
    subject_id: Uuid,
    label: &str,
) -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: format!(
                "INSERT INTO ann_write_log \
                 (namespace, embedding_model, kind, field, subject_id, op) \
                 SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                 FROM {table} WHERE namespace = ?1 AND subject_id = ?2"
            ),
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
    statements.extend(purge_fts_document_statements(
        fts_table,
        namespace,
        subject_id,
        &format!("{label_prefix}-purge-fts"),
    ));
    for vec_table in vector_table_names(runtime) {
        if vector_table_exists(runtime, &vec_table).await? {
            statements.push(log_vector_row_delete_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-log-delete-vec-{vec_table}"),
            ));
            statements.push(purge_index_row_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-purge-vec-{vec_table}"),
            ));
            statements.push(purge_vector_provenance_statement(
                &vec_table,
                namespace,
                subject_id,
                &format!("{label_prefix}-purge-vec-provenance-{vec_table}"),
            ));
        }
    }
    Ok(())
}

/// Event-store append parity for the canonical handlers that emit a
/// lifecycle event after their row mutation: `update_entity` ->
/// `EntityUpdated`, `delete_entity` -> `EntityDeleted`, `delete_note` ->
/// `NoteDeleted`, `update_edge` -> `EdgeUpdated`, `delete_edge` ->
/// `EdgeDeleted`, `link` -> `LinkCreated`/`EdgeUpdated`, and
/// `update_note` -> `NoteUpdated`. See
/// `docs/api/atomic_prepare.md#event_append_statements` for why
/// this is a `PlanStatement` rather than a `PostCommitEffect`.
///
/// Invariant: returned statements are unguarded — appended after the plan's
/// own guarded row statement, so [`apply_plan`]'s stop-on-first-failure
/// contract means they are only reached once that row mutation's guard has
/// already held. Committing the event row atomically with the mutation it
/// describes strengthens canonical's guarantee: the non-atomic handlers write
/// the event in a separate transaction, ordered but not atomic with the row
/// mutation.
pub(crate) fn event_append_statements(
    token: &NamespaceToken,
    namespace: &str,
    verb: &str,
    kind: EventKind,
    substrate: SubstrateKind,
    target_id: Uuid,
    payload: Value,
) -> RuntimeResult<Vec<PlanStatement>> {
    let record_token = token
        .with_namespace(crate::Namespace::parse(namespace).map_err(|error| {
            RuntimeError::Internal(format!("event namespace invalid: {error}"))
        })?);
    let event = crate::EventAttribution::from_token(&record_token).stamp(
        khive_storage::event::Event::new(namespace.to_string(), verb, kind, substrate, "")
            .with_target(target_id)
            .with_payload(payload),
    );
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

pub(crate) async fn prepare_update_entity_plan_with_version(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    patch: crate::curation::EntityPatch,
    expected_version: Option<i64>,
) -> RuntimeResult<AtomicOpPlan> {
    prepare_update_entity_plan_with_version_and_type(
        runtime,
        token,
        id,
        patch,
        expected_version,
        None,
    )
    .await
}

/// Edge branch of `prepare_update`. Mirrors `KhiveRuntime::update_edge`'s
/// patch semantics: `relation`/`weight`/`properties` are the only applicable
/// fields, a changed `relation` is endpoint-validated first, `weight` is
/// range-checked, and `properties` REPLACES `metadata` wholesale (no merge).
/// See `docs/api/atomic_prepare.md#prepare_update_edge` for the DML-shape parity
/// detail with `update_edge`.
///
/// Invariant (symmetric relations `competes_with`/`composed_with`): this
/// function must never branch on a prepare-time conflict probe — a different
/// op in the same atomic unit could change the conflict landscape between
/// probe and commit, making any such branch stale by construction. It always
/// emits BOTH statements from [`edge_symmetric_delete_if_conflict_statement`]
/// and [`edge_symmetric_absorb_or_update_inplace_statement`], each carrying
/// its own commit-time `WHERE`/`CASE WHEN` predicate that re-evaluates the
/// conflict condition fresh inside the transaction. This function reads no
/// state to guess a surviving id; the plan instead carries `edge_natural_key`
/// so a post-commit caller derives the actual surviving id from the
/// committed row, never from a value computed before the rest of this atomic
/// unit has even run.
async fn prepare_update_edge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    mut edge: khive_storage::types::Edge,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    reject_inapplicable_update_fields(args, "edge")?;

    let expected_updated_at = edge.updated_at;
    let expected_deleted_at = edge.deleted_at;

    let relation_raw = optional_str(args, "relation");
    let weight = optional_f64(args, "weight")?;
    let properties = optional_properties(args, "properties")?;

    if let Some(ref p) = properties {
        crate::secret_gate::check_json_at(p, "edge", "properties")?;
    }
    crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;

    let namespace = edge.namespace.clone();
    let record_tok = token.with_namespace(
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

        // `updated_at` must strictly advance past the snapshot even when two
        // operations land inside one clock microsecond; saturating to
        // i64::MAX would let the CAS accept a write without advancing its
        // revision, so that is not a valid fallback (mirrors the note path).
        let minimum_updated_at_micros = expected_updated_at
            .timestamp_micros()
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "edge {id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?;
        let symmetric_updated_at_micros = now.timestamp_micros().max(minimum_updated_at_micros);
        let expected_deleted_at_micros = expected_deleted_at.map(|v| v.timestamp_micros());

        statements.push(PlanStatement {
            statement: edge_symmetric_delete_if_conflict_statement(
                &namespace,
                id,
                canon_src,
                canon_tgt,
                edge.relation,
                expected_updated_at.timestamp_micros(),
                expected_deleted_at_micros,
            ),
            guard: Some(AffectedRowGuard {
                expected_min: 0,
                expected_max: Some(1),
            }),
        });
        statements.push(PlanStatement {
            statement: edge_symmetric_absorb_or_update_inplace_statement(
                &namespace,
                id,
                canon_src,
                canon_tgt,
                edge.relation,
                edge.weight,
                symmetric_updated_at_micros,
                metadata_str.as_deref(),
                edge.target_backend.as_deref(),
                expected_updated_at.timestamp_micros(),
                expected_deleted_at_micros,
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
        // Non-symmetric: guarded replace of the read snapshot rather than
        // `graph.upsert_edge`'s unconditional natural-key upsert — a zero
        // affected-row result now means a concurrent writer moved this edge
        // between PREPARE and commit, and the atomic unit must roll back
        // instead of silently overwriting it.
        //
        // `updated_at` must strictly advance past the snapshot even when two
        // operations land inside one clock microsecond; saturating to
        // i64::MAX would let the CAS accept a write without advancing its
        // revision, so that is not a valid fallback (mirrors the note path).
        let minimum_updated_at_micros = expected_updated_at
            .timestamp_micros()
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "edge {id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?;
        let now_micros = now.timestamp_micros().max(minimum_updated_at_micros);
        edge.updated_at = chrono::DateTime::from_timestamp_micros(now_micros).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "edge {id}: computed updated_at {now_micros} is not a valid timestamp"
            ))
        })?;
        statements.push(PlanStatement {
            statement: edge_replace_if_unchanged_statement(
                &edge,
                expected_updated_at,
                expected_deleted_at,
            ),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }

    // Mirrors `update_edge`'s unconditional post-mutation `EdgeUpdated`
    // event append, keyed on the original `edge_id` the caller supplied:
    // canonical does the same (the event target is `edge_id`, not the
    // post-absorption surviving id).
    statements.extend(event_append_statements(
        token,
        &namespace,
        "update",
        EventKind::EdgeUpdated,
        SubstrateKind::Entity,
        id,
        serde_json::json!({"id": id, "namespace": namespace, "changed_fields": changed_fields}),
    )?);

    Ok(AtomicOpPlan::Update(Box::new(UpdatePlan {
        graph_effects: Vec::new(),
        note_vector_purge: None,
        note_embedding_inheritance: None,
        entity_guard: None,
        note_guard: None,
        target_id: id,
        statements,
        post_commit: PostCommitEffect::None,
        edge_natural_key,
        idempotent_noop: false,
    })))
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
    Entity {
        specific: Option<String>,
        entity_type: Option<String>,
    },
    Note {
        specific: Option<String>,
    },
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
    let actor = format!("{}:{}", token.actor().kind, token.actor().id);
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
                    ..
                }) if &entity.kind != expected => {
                    return Err(RuntimeError::NotFound(format!("{expected} {id}")));
                }
                Some(AtomicDeleteKind::Entity { .. }) => {}
                Some(AtomicDeleteKind::Note { .. }) => {
                    return Err(RuntimeError::NotFound(format!("note {id}")));
                }
                Some(AtomicDeleteKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            if let Some(AtomicDeleteKind::Entity {
                entity_type: Some(expected),
                ..
            }) = &expected_kind
            {
                if entity
                    .entity_type
                    .as_deref()
                    .is_some_and(|actual| actual != expected.as_str())
                {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
            }
            refuse_pack_registry_tags(&entity.tags, "delete")?;
            let namespace = entity.namespace.clone();
            // Storage parity: `entity_soft_delete_statement`/
            // `entity_hard_delete_statement` are the SAME khive-db builders
            // khive-db's own `SqlEntityStore::delete_entity` calls — no DML
            // text is hand-duplicated here.
            let mut statements = if hard {
                vec![
                    PlanStatement {
                        statement: delete_record_attachments_statement(
                            id,
                            AttachmentSubstrate::Entity,
                        ),
                        guard: None,
                    },
                    PlanStatement {
                        statement: entity_hard_delete_statement(id),
                        guard: Some(AffectedRowGuard::exactly(1)),
                    },
                ]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: entity_soft_delete_statement(id, deleted_at),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.extend(
                    hard_delete_lineage_warning_statements(
                        &namespace,
                        &actor,
                        id,
                        SubstrateKind::Entity,
                    )
                    .into_iter()
                    .map(|statement| PlanStatement {
                        statement,
                        guard: None,
                    }),
                );
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
                token,
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
                }) if &note.kind != expected => {
                    return Err(RuntimeError::NotFound(format!("{expected} {id}")));
                }
                Some(AtomicDeleteKind::Note { .. }) => {}
                Some(AtomicDeleteKind::Entity { .. }) => {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
                Some(AtomicDeleteKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            if let Some(error) = runtime.stream_member_error(&note).await? {
                return Err(error);
            }
            let namespace = note.namespace.clone();
            // Storage parity: `note_soft_delete_statement`/
            // `note_hard_delete_statement` are the SAME khive-db builders
            // khive-db's own `SqlNoteStore::delete_note` calls.
            let mut statements = if hard {
                vec![
                    PlanStatement {
                        statement: delete_record_attachments_statement(
                            id,
                            AttachmentSubstrate::Note,
                        ),
                        guard: None,
                    },
                    PlanStatement {
                        statement: note_hard_delete_statement(id),
                        guard: Some(AffectedRowGuard::exactly(1)),
                    },
                ]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: note_soft_delete_statement(id, deleted_at),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.extend(
                    hard_delete_lineage_warning_statements(
                        &namespace,
                        &actor,
                        id,
                        SubstrateKind::Note,
                    )
                    .into_iter()
                    .map(|statement| PlanStatement {
                        statement,
                        guard: None,
                    }),
                );
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
                token,
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
                    Some(edge) => prepare_delete_edge(token, id, edge, hard, &actor).await,
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
    token: &NamespaceToken,
    id: Uuid,
    edge: khive_storage::types::Edge,
    hard: bool,
    actor: &str,
) -> RuntimeResult<AtomicOpPlan> {
    let namespace = edge.namespace.clone();
    let mut statements: Vec<PlanStatement> = Vec::new();

    if hard {
        statements.extend(
            hard_delete_lineage_warning_statements(&namespace, actor, id, SubstrateKind::Entity)
                .into_iter()
                .map(|statement| PlanStatement {
                    statement,
                    guard: None,
                }),
        );
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
        token,
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
    let resurrect = match obj(args)?.get("resurrect") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(other) => {
            return Err(RuntimeError::InvalidInput(format!(
                "resurrect must be a boolean, got: {other}"
            )))
        }
    };

    // Top-level `dependency_kind` param merges into `metadata`: only fills
    // the key when metadata doesn't already carry one. Calls the same
    // `khive_runtime::merge_entry_metadata` `khive-pack-kg`'s canonical
    // `handle_link` calls, so both sides depend on one function instead of
    // each maintaining their own copy.
    let mut metadata = crate::merge_entry_metadata(
        metadata,
        optional_str(args, "dependency_kind").map(String::from),
    )?;
    validate_edge_metadata(relation, metadata.as_ref())?;

    validate_edge_weight(weight)?;
    let (source_kind, target_kind) = runtime
        .validate_edge_relation_endpoints(token, source_id, target_id, relation)
        .await?;

    let (canon_source, canon_target) = canonical_edge_endpoints(relation, source_id, target_id);
    let (source_kind, target_kind) =
        canonical_edge_endpoint_kinds(source_id, canon_source, source_kind, target_kind);

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
    let namespace = token.namespace().as_str().to_string();
    let previous = runtime
        .get_edge_by_natural_key_including_deleted(
            token,
            &namespace,
            canon_source,
            canon_target,
            relation,
        )
        .await?;
    if let Some(edge) = previous.as_ref() {
        if edge.deleted_at.is_some() && !resurrect {
            return Err(RuntimeError::InvalidInput(format!(
                "edge natural key is soft-deleted; pass resurrect=true to link explicitly: {}",
                Uuid::from(edge.id)
            )));
        }
    }

    let disposition = match previous.as_ref() {
        None => EdgeUpsertDisposition::Created,
        Some(edge) if edge.deleted_at.is_some() => EdgeUpsertDisposition::Resurrected,
        Some(_) => EdgeUpsertDisposition::Updated,
    };
    let edge_id = previous
        .as_ref()
        .map(|edge| Uuid::from(edge.id))
        .unwrap_or_else(Uuid::new_v4);
    let now = previous.as_ref().map_or_else(
        || chrono::Utc::now().timestamp_micros(),
        |edge| {
            chrono::Utc::now()
                .timestamp_micros()
                .max(edge.updated_at.timestamp_micros().saturating_add(1))
        },
    );
    let metadata_str = metadata
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_default());

    // The guarded mutation closes both atomic seams: endpoints are re-probed
    // inside the transaction, and the natural-key row must still match the
    // prepare snapshot. That makes the disposition used below truthful.
    let statement = match previous.as_ref() {
        None => edge_insert_new_guarded_by_endpoints_statement(
            &namespace,
            edge_id,
            canon_source,
            canon_target,
            relation,
            weight,
            now,
            metadata_str.as_deref(),
        ),
        Some(edge) => edge_link_replace_if_unchanged_and_endpoints_exist_statement(
            edge,
            weight,
            now,
            metadata_str.as_deref(),
        ),
    };
    let mut statements = vec![PlanStatement {
        statement,
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    let kind = match disposition {
        EdgeUpsertDisposition::Created => EventKind::LinkCreated,
        EdgeUpsertDisposition::Updated | EdgeUpsertDisposition::Resurrected => {
            EventKind::EdgeUpdated
        }
    };
    let mut payload = serde_json::json!({
        "id": edge_id,
        "namespace": namespace,
        "mutation": disposition.name(),
        "source_id": canon_source,
        "target_id": canon_target,
        "relation": relation,
        "weight": weight,
        "metadata": metadata,
        "previous": previous,
    });
    if kind == EventKind::LinkCreated {
        payload["source_kind"] = serde_json::json!(source_kind.name());
        payload["target_kind"] = serde_json::json!(target_kind.name());
    }
    statements.extend(event_append_statements(
        token,
        &namespace,
        "link",
        kind,
        SubstrateKind::Entity,
        edge_id,
        payload,
    )?);

    Ok(AtomicOpPlan::Link(LinkPlan {
        source_id: canon_source,
        target_id: canon_target,
        statements,
        disposition,
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
    let into_entity = entities
        .get_entity(into_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {into_id}")))?;
    let from_entity = entities
        .get_entity(from_id)
        .await?
        .ok_or_else(|| RuntimeError::NotFound(format!("entity {from_id}")))?;
    refuse_pack_registry_tags(&into_entity.tags, "merge")?;
    refuse_pack_registry_tags(&from_entity.tags, "merge")?;

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
            sql: "UPDATE entities SET deleted_at = ?1, merged_into = ?2, version = version + 1 \
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

mod embedding_outcome;
pub use embedding_outcome::{
    apply_post_commit_effects_with_failures, PostCommitEffectsReport, PostCommitEmbeddingOutcome,
    ReindexModelFailure, ReindexModelStage,
};

/// Run every deferred [`PostCommitEffect`] after a committed atomic unit.
pub async fn apply_post_commit_effects(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: CommittedPostCommitEffects,
) -> RuntimeResult<()> {
    apply_post_commit_effects_with_report(runtime, token, effects)
        .await
        .map(|_| ())
}

/// Embedding-reporting form of [`apply_post_commit_effects`]. Re-fetches each
/// target's now-committed row outside any transaction and reuses the existing
/// `reindex_entity`/`reindex_note` (FTS + embedding, same as the non-atomic
/// path) for exact parity. Returns the typed embedding outcome for each reindex
/// effect so callers can preserve truncation advisories and partial model
/// failures instead of discarding them after commit. Model failures remain
/// best-effort; lexical indexing and excluded-model cleanup errors propagate.
/// When any effect fails, the other effects' outcomes are dropped with the
/// returned error; [`apply_post_commit_effects_with_failures`] keeps both.
pub async fn apply_post_commit_effects_with_report(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effects: CommittedPostCommitEffects,
) -> RuntimeResult<Vec<PostCommitEmbeddingOutcome>> {
    let report = apply_post_commit_effects_with_failures(runtime, token, effects).await;
    match report.failure_error() {
        Some(error) => Err(error),
        None => Ok(report.outcomes),
    }
}

async fn apply_one_post_commit_effect(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    effect: PostCommitEffect,
) -> RuntimeResult<Option<PostCommitEmbeddingOutcome>> {
    match effect {
        PostCommitEffect::None => Ok(None),
        PostCommitEffect::NoteChanged { note_id, kind } => {
            runtime.fire_note_mutation_hook(&kind, note_id).await;
            Ok(None)
        }
        PostCommitEffect::ReindexEntity { entity_id } => {
            let Some(entity) = runtime.entities(token)?.get_entity(entity_id).await? else {
                return Ok(None);
            };
            let truncation = runtime.reindex_entity(token, &entity).await?;
            Ok(Some(PostCommitEmbeddingOutcome {
                effect: PostCommitEffect::ReindexEntity { entity_id },
                truncation,
                failures: Vec::new(),
            }))
        }
        PostCommitEffect::ReindexNote { note_id, version } => {
            note_reindex_effect::apply(runtime, token, note_id, version).await
        }
        PostCommitEffect::NoteDeleted { note_id, kind } => {
            // The committed row may already be gone; use the captured kind.
            runtime.fire_note_mutation_hook(&kind, note_id).await;
            Ok(None)
        }
        PostCommitEffect::GtdAudit { .. } => {
            // The kkernel caller owns the GTD pack's separate audit side write.
            Ok(None)
        }
    }
}

#[cfg(test)]
#[path = "atomic_prepare_tests.rs"]
mod tests;
