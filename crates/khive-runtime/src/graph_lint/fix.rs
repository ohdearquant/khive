use super::*;
use crate::atomic_runner::{
    run_prepared_atomic_unit, AtomicOpPlan, PreparedAtomicError, PreparedAtomicOp,
    PreparedAtomicOutcome,
};
use crate::{KhiveRuntime, Namespace, NamespaceToken, RuntimeError, RuntimeResult, VerbRegistry};
use khive_storage::{SqlStatement, StorageError};
use serde_json::json;
use uuid::Uuid;

struct PreparedFix {
    owner: KhiveRuntime,
    token: NamespaceToken,
    guards: Vec<SqlStatement>,
    plan: AtomicOpPlan,
    committed: CommittedFixAction,
}
enum ActionRefusal {
    Conflict(String),
    Failed(RuntimeError),
}

enum Candidate {
    Ready(Box<PreparedFix>),
    Unsupported(String),
    Conflict(String),
}
impl LintCapture {
    fn unique_owner(&self, ids: &[Uuid]) -> Option<usize> {
        let owner = *self.record_owners.get(ids.first()?)?.first()?;
        ids.iter()
            .all(|id| {
                self.record_owners
                    .get(id)
                    .is_some_and(|owners| owners.as_slice() == [owner])
            })
            .then_some(owner)
    }
    fn tombstone_guard(&self, id: Uuid) -> Option<SqlStatement> {
        if let Some(entity) = self
            .all_entities
            .get(&id)
            .filter(|entity| entity.deleted_at.is_some())
        {
            return Some(khive_db::graph_lint::entity_witness(entity));
        }
        if let Some(note) = self
            .all_notes
            .get(&id)
            .filter(|note| note.deleted_at.is_some())
        {
            return Some(khive_db::graph_lint::note_witness(note));
        }
        self.all_edges
            .get(&id)
            .filter(|edge| edge.deleted_at.is_some())
            .and_then(|_| self.edge_guards.get(&id).cloned())
    }
    async fn prepare_fix(
        &self,
        registry: &VerbRegistry,
        token: &NamespaceToken,
        identity: &ActionIdentity,
        violation: &LintViolation,
    ) -> RuntimeResult<Candidate> {
        let id = violation.subject.id();
        let captured_subject = match &violation.subject {
            LintSubject::Entity {
                namespace,
                entity_id,
                entity_name,
                entity_kind,
            } => self.entities.iter().any(|entity| {
                entity.id == *entity_id
                    && entity.namespace == *namespace
                    && entity.name == *entity_name
                    && entity.kind == *entity_kind
            }),
            LintSubject::Edge { namespace, edge_id } => self.edges.iter().any(|inspected| {
                Uuid::from(inspected.edge.id) == *edge_id && inspected.edge.namespace == *namespace
            }),
            LintSubject::Note { .. } => false,
        };
        if !captured_subject {
            return Err(RuntimeError::InvalidInput(
                "lint action subject was not in the visible capture".into(),
            ));
        }

        let unsupported = || {
            Candidate::Unsupported(
                "the observed action has no unique same-database guarded owner".into(),
            )
        };
        let ns = Namespace::parse(violation.subject.namespace())
            .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
        if !token.visible_namespaces().contains(&ns) {
            return Err(RuntimeError::InvalidInput(
                "lint action is outside its captured visible scope".into(),
            ));
        }
        let write_token = token.with_namespace(ns).with_request_id(token.request_id());
        let (owner, guards, verb, args) = match identity.action {
            FixAction::SoftDeleteEdge => {
                let Some(edge) = self
                    .all_edges
                    .get(&id)
                    .filter(|edge| edge.deleted_at.is_none())
                else {
                    return Ok(Candidate::Conflict("captured edge is not live".into()));
                };
                let Some((owner, endpoint_guard)) = [edge.source_id, edge.target_id]
                    .into_iter()
                    .find_map(|endpoint| {
                        let owner = self.unique_owner(&[id, endpoint])?;
                        self.tombstone_guard(endpoint).map(|guard| (owner, guard))
                    })
                else {
                    return Ok(unsupported());
                };
                (
                    owner,
                    vec![self.edge_guards[&id].clone(), endpoint_guard],
                    "delete",
                    json!({"id":id, "hard":false}),
                )
            }
            FixAction::ProposeDuplicateMerge => {
                let members = violation.member_ids.as_deref().ok_or_else(|| {
                    RuntimeError::Internal("duplicate lint violation lacks members".into())
                })?;
                if members.len() < 2 {
                    return Err(RuntimeError::Internal(
                        "duplicate lint group needs two distinct members".into(),
                    ));
                }
                let mut distinct = members.to_vec();
                distinct.sort();
                distinct.dedup();
                let first = self.all_entities.get(&id).ok_or_else(|| {
                    RuntimeError::Internal("duplicate representative was not captured".into())
                })?;
                if distinct.len() != members.len()
                    || distinct.first() != Some(&id)
                    || members.iter().any(|member| {
                        self.all_entities.get(member).is_none_or(|entity| {
                            entity.namespace != first.namespace
                                || entity.kind != first.kind
                                || entity.name != first.name
                        })
                    })
                {
                    return Err(RuntimeError::InvalidInput(
                        "lint duplicate group does not match the captured identity".into(),
                    ));
                }
                let Some(owner) = self.unique_owner(members) else {
                    return Ok(unsupported());
                };
                let mut guards = Vec::new();
                for member in members {
                    let entity = self.all_entities.get(member).ok_or_else(|| {
                        RuntimeError::Internal("lint duplicate member was not captured".into())
                    })?;
                    if entity.deleted_at.is_some() {
                        return Ok(Candidate::Conflict(
                            "captured duplicate member is not live".into(),
                        ));
                    }
                    guards.push(khive_db::graph_lint::entity_witness(entity));
                }
                let content = format!("Possible duplicate entities in namespace {}: {}. Review these records before deciding whether to merge them; no winner has been selected.", violation.subject.namespace(), members.iter().map(Uuid::to_string).collect::<Vec<_>>().join(", "));
                (
                    owner,
                    guards,
                    "create",
                    json!({"kind":"question", "content":content, "properties":{"lint_rule":"duplicate-name", "member_ids":members}}),
                )
            }
            FixAction::MarkDeprecated => {
                let Some(entity) = self.all_entities.get(&id).filter(|entity| {
                    entity.deleted_at.is_none()
                        && entity
                            .properties
                            .as_ref()
                            .and_then(|properties| properties.get("status"))
                            .and_then(serde_json::Value::as_str)
                            != Some("deprecated")
                }) else {
                    return Ok(Candidate::Conflict(
                        "captured entity is not an unmarked live target".into(),
                    ));
                };
                let witness = self.edges.iter().find_map(|inspected| {
                    let edge = &inspected.edge;
                    if edge.target_id != id
                        || edge.relation != khive_storage::EdgeRelation::Supersedes
                        || !inspected.source_live
                        || inspected.contract_valid != Some(true)
                    {
                        return None;
                    }
                    let source = self.all_entities.get(&edge.source_id)?;
                    let owner = self.unique_owner(&[id, Uuid::from(edge.id), source.id])?;
                    Some((owner, edge, source))
                });
                let Some((owner, edge, source)) = witness else {
                    return Ok(unsupported());
                };
                (
                    owner,
                    vec![
                        khive_db::graph_lint::entity_witness(entity),
                        self.edge_guards[&Uuid::from(edge.id)].clone(),
                        khive_db::graph_lint::entity_witness(source),
                    ],
                    "update",
                    json!({"id":id,"properties":{"status":"deprecated"}}),
                )
            }
        };
        let runtime = &self.owners[owner];
        {
            let mut reader = runtime.sql().reader().await?;
            for guard in &guards {
                if reader.query_all(guard.clone()).await?.len() != 1 {
                    return Ok(Candidate::Conflict(
                        "an observed lint predicate changed before preparation".into(),
                    ));
                }
            }
        }
        // Consult the normal verb gate with the effective target and original
        // principal. Its ordinary audit obligations remain part of preparation.
        registry
            .authorize_effective_kg_read(&write_token, verb, args.clone(), id)
            .await?;
        let mut plan = match identity.action {
            FixAction::SoftDeleteEdge => {
                crate::atomic_prepare::prepare_delete(
                    runtime,
                    &write_token,
                    &args,
                    Some(crate::atomic_prepare::AtomicDeleteKind::Edge),
                )
                .await?
            }
            FixAction::ProposeDuplicateMerge => {
                crate::atomic_prepare::prepare_add_note(runtime, &write_token, &args).await?
            }
            FixAction::MarkDeprecated => {
                crate::atomic_prepare::prepare_update_entity_plan_with_version(
                    runtime,
                    &write_token,
                    id,
                    crate::curation::EntityPatch {
                        properties: Some(json!({"status":"deprecated"})),
                        ..Default::default()
                    },
                    Some(self.all_entities[&id].version),
                )
                .await?
            }
        };
        if let AtomicOpPlan::AddNote(note) = &mut plan {
            note.statements.extend(crate::atomic_prepare::event_append_statements(
                &write_token, write_token.namespace().as_str(), "create",
                khive_types::EventKind::NoteCreated, khive_types::SubstrateKind::Note,
                note.note_id, json!({"id":note.note_id, "namespace":write_token.namespace().as_str(), "kind":"question", "salience":null}),
            )?);
        }
        let committed = match &plan {
            AtomicOpPlan::AddNote(plan) => CommittedFixAction::ProposeDuplicateMerge {
                violation_index: identity.violation_index,
                question_note_id: plan.note_id,
            },
            AtomicOpPlan::Delete(_) => CommittedFixAction::SoftDeleteEdge {
                violation_index: identity.violation_index,
            },
            AtomicOpPlan::Update(_) => CommittedFixAction::MarkDeprecated {
                violation_index: identity.violation_index,
            },
            _ => return Err(RuntimeError::Internal("unexpected graph lint plan".into())),
        };
        Ok(Candidate::Ready(Box::new(PreparedFix {
            owner: runtime.clone(),
            token: write_token,
            guards,
            plan,
            committed,
        })))
    }

    /// Prepare every candidate before the first domain write, then commit each
    /// independent guarded action once. No failure is retried or replayed.
    pub async fn apply_fixes(
        &self,
        registry: &VerbRegistry,
        token: &NamespaceToken,
        mut report: LintReport,
    ) -> RuntimeResult<(LintReport, Vec<FixReceipt>)> {
        if token.actor() != self.token.actor()
            || token.namespace() != self.token.namespace()
            || token.visible_namespaces() != self.token.visible_namespaces()
            || token.request_id() != self.token.request_id()
            || token.process_ref() != self.token.process_ref()
        {
            return Err(RuntimeError::InvalidInput(
                "lint capture belongs to a different request scope".into(),
            ));
        }
        let mut prepared = Vec::new();
        for (index, violation) in report.violations.iter().enumerate() {
            let action = match violation.rule {
                LintRuleId::DanglingEdge => FixAction::SoftDeleteEdge,
                LintRuleId::DuplicateName => FixAction::ProposeDuplicateMerge,
                LintRuleId::SupersededNotMarked => FixAction::MarkDeprecated,
                _ => continue,
            };
            let identity = ActionIdentity {
                violation_index: index as u64,
                action,
            };
            let candidate = self
                .prepare_fix(registry, token, &identity, violation)
                .await;
            prepared.push((identity, candidate));
        }
        report.summary.fixed = Some(0);
        let mut receipts = Vec::new();
        for (identity, candidate) in prepared {
            let candidate = candidate.map_err(|source| {
                failure(
                    source,
                    LintFixOutcome::Failed,
                    identity.clone(),
                    &receipts,
                    &report,
                )
            })?;
            match candidate {
                Candidate::Unsupported(reason) => {
                    receipts.push(FixReceipt::Unsupported { identity, reason })
                }
                Candidate::Conflict(reason) => {
                    receipts.push(FixReceipt::Conflict { identity, reason })
                }
                Candidate::Ready(prepared) => {
                    let PreparedFix {
                        owner,
                        token,
                        guards,
                        plan,
                        committed,
                    } = *prepared;
                    let operation: PreparedAtomicOp<(), ActionRefusal> = Box::new(move |writer| {
                        Box::pin(async move {
                            for guard in guards {
                                if writer.query_all(guard).await?.len() != 1 {
                                    return Err(PreparedAtomicError::Refused {
                                        failure: ActionRefusal::Conflict(
                                            "an observed lint predicate changed".into(),
                                        ),
                                        message: "graph lint witness conflict".into(),
                                    });
                                }
                            }
                            let applied = crate::atomic_runner::apply_plan_preserving_storage(
                                writer, &plan, true,
                            )
                            .await
                            .map_err(|failure| match failure {
                                crate::atomic_runner::PlanExecutionFailure::Storage {
                                    source,
                                    ..
                                } => PreparedAtomicError::Storage(source),
                                crate::atomic_runner::PlanExecutionFailure::Operation(failure) => {
                                    let failure = match failure {
                                        crate::atomic_runner::AtomicOpFailure::SqlError {
                                            message,
                                            ..
                                        } => ActionRefusal::Failed(RuntimeError::Internal(message)),
                                        _ => ActionRefusal::Conflict(
                                            "an ordinary prepared mutation guard changed".into(),
                                        ),
                                    };
                                    PreparedAtomicError::Refused {
                                        failure,
                                        message: "graph lint plan refused".into(),
                                    }
                                }
                            })?;
                            Ok(((), applied.effect.into_iter().collect()))
                        })
                    });
                    match run_prepared_atomic_unit(action_access(&owner).as_ref(), operation).await
                    {
                        Ok(PreparedAtomicOutcome::RolledBack(ActionRefusal::Conflict(reason))) => {
                            receipts.push(FixReceipt::Conflict { identity, reason })
                        }
                        Ok(PreparedAtomicOutcome::RolledBack(ActionRefusal::Failed(source))) => {
                            return Err(failure(
                                source,
                                LintFixOutcome::Failed,
                                identity,
                                &receipts,
                                &report,
                            ))
                        }
                        Ok(PreparedAtomicOutcome::Committed { post_commit, .. }) => {
                            receipts.push(FixReceipt::Committed { action: committed });
                            report.summary.fixed = Some(
                                receipts
                                    .iter()
                                    .filter(|receipt| receipt.is_committed())
                                    .count() as u64,
                            );
                            // Each prepared lint action has at most one ordinary
                            // effect. Preserve its typed cause rather than the
                            // multi-effect compatibility API's string aggregate.
                            for effect in post_commit.into_effects() {
                                if let Err(source) =
                                    crate::atomic_prepare::apply_one_post_commit_effect(
                                        &owner, &token, effect,
                                    )
                                    .await
                                {
                                    return Err(failure(
                                        source,
                                        LintFixOutcome::Failed,
                                        identity,
                                        &receipts,
                                        &report,
                                    ));
                                }
                            }
                        }
                        Err(source) => {
                            let outcome = if unknown(&source) {
                                LintFixOutcome::Unknown
                            } else {
                                LintFixOutcome::Failed
                            };
                            return Err(failure(
                                source.into(),
                                outcome,
                                identity,
                                &receipts,
                                &report,
                            ));
                        }
                    }
                }
            }
        }
        Ok((report, receipts))
    }
}
fn unknown(error: &StorageError) -> bool {
    if error
        .sqlite_write_failure()
        .is_some_and(|failure| failure.settlement_unknown)
    {
        return true;
    }
    match error.without_sqlite_write_stage() {
        StorageError::WriterTaskRequestFailed { request_state, .. }
        | StorageError::WriterTaskTerminated { request_state, .. } => {
            *request_state == khive_storage::WriterTaskRequestState::SideEffectsUnknown
        }
        _ => false,
    }
}
fn failure(
    source: RuntimeError,
    outcome: LintFixOutcome,
    failed_action: ActionIdentity,
    receipts: &[FixReceipt],
    report: &LintReport,
) -> RuntimeError {
    RuntimeError::LintFix {
        source: Box::new(source),
        detail: Box::new(LintFixFailureDetail {
            outcome,
            failed_action,
            confirmed_receipts: receipts.to_vec(),
            report: report.clone(),
        }),
    }
}

fn action_access(owner: &KhiveRuntime) -> std::sync::Arc<dyn khive_storage::SqlAccess> {
    #[cfg(test)]
    if let Ok(access) = TEST_ACTION_ACCESS.try_with(std::sync::Arc::clone) {
        return access;
    }
    owner.sql()
}
#[cfg(test)]
tokio::task_local! {
    pub(super) static TEST_ACTION_ACCESS: std::sync::Arc<dyn khive_storage::SqlAccess>;
}
