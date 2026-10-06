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
// post-commit effects
// ---------------------------------------------------------------------------

mod embedding_outcome;
pub use embedding_outcome::{
    apply_post_commit_effects_with_failures, PostCommitEffectsReport, PostCommitEmbeddingOutcome,
    ReindexModelFailure, ReindexModelStage,
};

mod link_merge;
use link_merge::{parse_edge_relation, prepare_link, prepare_merge};

mod post_commit;
use post_commit::apply_one_post_commit_effect;
pub use post_commit::{apply_post_commit_effects, apply_post_commit_effects_with_report};

mod edge_delete;
pub use edge_delete::{prepare_delete, AtomicDeleteKind};
use edge_delete::{prepare_delete_edge, prepare_update_edge};

#[cfg(test)]
#[path = "atomic_prepare_tests.rs"]
mod tests;
