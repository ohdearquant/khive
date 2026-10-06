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

mod index_purge;

pub(crate) use index_purge::event_append_statements;
use index_purge::push_index_purge_statements;

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
mod patch_helpers;

use patch_helpers::{
    entity_name_patch, obj, optional_create_string, optional_entity_type_patch, optional_f64,
    optional_f64_patch, optional_properties, optional_str, optional_string_patch, optional_tags,
    refuse_pack_registry_tags, require_str, require_uuid,
};

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

mod post_commit;
use post_commit::apply_one_post_commit_effect;
pub use post_commit::{apply_post_commit_effects, apply_post_commit_effects_with_report};

mod edge_delete;
pub use edge_delete::{prepare_delete, AtomicDeleteKind};
use edge_delete::{prepare_delete_edge, prepare_update_edge};

#[cfg(test)]
#[path = "atomic_prepare_tests.rs"]
mod tests;
