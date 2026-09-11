//! Shared note creation with explicit key-publication order.

use khive_storage::types::{Edge, LinkId, SqlValue};
use khive_storage::{EdgeRelation, SqlStatement};
use uuid::Uuid;

use crate::atomic_message::{
    prepare_atomic_notes, AtomicNoteOptions, AtomicNoteSpec, PreparedAtomicNotes,
};
use crate::atomic_plan::{AffectedRowGuard, PlanStatement};
use crate::atomic_runner::AtomicOpPlan;
use crate::{KhiveRuntime, RuntimeError, RuntimeResult};

pub(crate) enum KeyPublication {
    AtInsert,
    AfterDependents,
}

pub(crate) const KEY_CLAIM: &str = "memory-key-claim";

pub(crate) async fn prepare_note_create(
    runtime: &KhiveRuntime,
    spec: AtomicNoteSpec<'_>,
    mut options: AtomicNoteOptions<'_>,
    annotates: &[Uuid],
    publication: KeyPublication,
) -> RuntimeResult<(PreparedAtomicNotes, Vec<Uuid>)> {
    for target in annotates {
        if !runtime.substrate_exists_by_id(spec.token, *target).await? {
            return Err(RuntimeError::NotFound(format!(
                "create_note annotates target {target} not found"
            )));
        }
    }
    let claim_last = match publication {
        KeyPublication::AtInsert => None,
        KeyPublication::AfterDependents => Some(
            options
                .key
                .take()
                .ok_or_else(|| {
                    RuntimeError::InvalidInput("deferred key publication requires a key".into())
                })?
                .to_owned(),
        ),
    };
    let mut prepared = prepare_atomic_notes(runtime, vec![spec], options).await?;
    let note = &prepared.notes[0];
    let AtomicOpPlan::AddNote(plan) = &mut prepared.plans[0] else {
        return Err(RuntimeError::Internal(
            "expected prepared note creation".into(),
        ));
    };
    if claim_last.is_some() {
        plan.statements[0].statement =
            khive_db::stores::note::note_insert_if_absent_statement(note);
    }
    let mut annotation_ids = Vec::with_capacity(annotates.len());
    for target in annotates {
        let now = chrono::Utc::now();
        let id = Uuid::new_v4();
        let edge = Edge {
            id: LinkId::from(id),
            namespace: note.namespace.clone(),
            source_id: note.id,
            target_id: *target,
            relation: EdgeRelation::Annotates,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        };
        plan.statements.push(PlanStatement {
            statement: khive_db::stores::graph::edge_insert_only_guarded_by_endpoints_statement(
                &edge,
            ),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
        annotation_ids.push(id);
    }
    if let Some(key) = claim_last {
        plan.statements.push(PlanStatement {
            statement: SqlStatement {
                sql: "UPDATE OR IGNORE notes SET key = ?1 WHERE id = ?2 AND namespace = ?3 \
                      AND kind = ?4 AND key IS NULL AND deleted_at IS NULL"
                    .into(),
                params: vec![
                    SqlValue::Text(key),
                    SqlValue::Text(note.id.to_string()),
                    SqlValue::Text(note.namespace.clone()),
                    SqlValue::Text(note.kind.clone()),
                ],
                label: Some(KEY_CLAIM.into()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }
    Ok((prepared, annotation_ids))
}
