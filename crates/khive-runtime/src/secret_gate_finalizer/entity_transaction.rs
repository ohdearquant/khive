//! Entity finalization inside the caller's existing atomic unit.

use std::sync::{Arc, Mutex};

use khive_db::stores::{
    entity::{entity_insert_if_absent_statement, entity_replace_if_unchanged_statement},
    event::event_insert_statements,
};
use khive_storage::{Entity, Event, SqlAccess, SqlStatement, SqlValue, SqlWriter, StorageError};
use khive_types::{EventKind, EventOutcome, SubstrateKind};

use super::entity_admission::{observe, PreparedEntityExemption};
use super::log_sink::{FinalizerAuditGap, LogSink, TracingLogSink};
use super::outcome::{
    ExemptionCommit, FailureClass, FailureDiagnostic, FinalizerOutcome, Substrate,
};
use crate::atomic_plan::{AffectedRowGuard, PlanStatement, PostCommitEffect};
use crate::atomic_runner::AtomicOpFailure;
use crate::entity_write::EntityWriteGuard;
use crate::{RuntimeError, RuntimeResult};

/// The existing conditional-write semantics retained by manifest admission.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // Own the captured snapshot without an additional caller allocation.
pub enum EntityCandidateMutation {
    CreateIfAbsent,
    ReplaceIfUnchanged {
        expected: Entity,
        expected_version: Option<i64>,
    },
}

#[derive(Debug)]
enum RecordedFailure {
    Contention,
    Finalization {
        diagnostic: FailureDiagnostic,
        source: Option<Box<StorageError>>,
    },
}

/// Opaque, runtime-constructed record/stamp/audit plan. There is no public
/// constructor accepting SQL or caller-supplied exemption state.
#[derive(Debug)]
pub struct EntityFinalizationPlan {
    prepared: PreparedEntityExemption,
    row: SqlStatement,
    guard: Option<EntityWriteGuard>,
    required: Vec<PlanStatement>,
    success_audit: Vec<SqlStatement>,
    post_commit: PostCommitEffect,
    replacing: bool,
    committed_version: i64,
    failure: Arc<Mutex<Option<RecordedFailure>>>,
}

impl Clone for EntityFinalizationPlan {
    fn clone(&self) -> Self {
        let mut plan = self.for_settlement();
        // A replay/concurrent invocation owns a separate failure record.
        plan.failure = Arc::new(Mutex::new(None));
        plan
    }
}

impl PreparedEntityExemption {
    pub(crate) fn into_plan(
        self,
        mutation: EntityCandidateMutation,
        required: Vec<PlanStatement>,
        post_commit: PostCommitEffect,
    ) -> RuntimeResult<EntityFinalizationPlan> {
        let (row, guard, replacing, committed_version) = match mutation {
            EntityCandidateMutation::CreateIfAbsent => (
                entity_insert_if_absent_statement(&self.entity),
                None,
                false,
                1,
            ),
            EntityCandidateMutation::ReplaceIfUnchanged {
                expected,
                expected_version,
            } => {
                crate::entity_write::validate_expected_version(expected_version)?;
                if expected.id != self.entity.id
                    || expected.version != self.entity.version
                    || expected.namespace != self.entity.namespace
                    || expected.created_at != self.entity.created_at
                {
                    return Err(RuntimeError::InvalidInput(
                        "entity finalization CAS snapshot mismatch".into(),
                    ));
                }
                let committed_version = expected
                    .version
                    .checked_add(1)
                    .ok_or_else(|| RuntimeError::InvalidInput("entity version exhausted".into()))?;
                let mut row = entity_replace_if_unchanged_statement(
                    &self.entity,
                    expected.updated_at,
                    expected.deleted_at,
                );
                // Reuse the canonical CAS builder, binding its captured target
                // namespace without depending on its current parameter count.
                row.sql
                    .push_str(&format!(" AND namespace = ?{}", row.params.len() + 1));
                row.params.push(SqlValue::Text(expected.namespace));
                row.sql
                    .push_str(&format!(" AND created_at = ?{}", row.params.len() + 1));
                row.params.push(SqlValue::Integer(expected.created_at));
                let guard = expected_version.map(|expected_version| EntityWriteGuard {
                    id: expected.id,
                    expected_version,
                });
                (row, guard, true, committed_version)
            }
        };
        let event = self.attribution.stamp(
            Event::new(
                &self.entity.namespace,
                self.entry.verb(),
                EventKind::Audit,
                SubstrateKind::Entity,
                "",
            )
            .with_target(self.entity.id)
            .with_payload(serde_json::json!({
                "mechanism": "content-sha256-manifest-v1",
                "digest_sha256": self.digest_sha256,
                "field_scope": self.field_scope,
                "manifest_id": self.snapshot.manifest_id(),
                "canonical_verb": self.entry.verb(),
                "namespace": self.entity.namespace,
                "overridden_detector": self.overridden_detector,
                "outcome": "exempted",
                "record_id": self.entity.id,
                "entry_point": self.entry.family(replacing),
            })),
        );
        let mut event = event;
        event.payload["actor"] = event.actor.clone().into();
        let success_audit = event_insert_statements(&event).map_err(|error| {
            RuntimeError::Internal(format!("finalizer event preparation: {error}"))
        })?;
        Ok(EntityFinalizationPlan {
            prepared: self,
            row,
            guard,
            required,
            success_audit,
            post_commit,
            replacing,
            committed_version,
            failure: Arc::new(Mutex::new(None)),
        })
    }
}

impl EntityFinalizationPlan {
    pub(crate) fn for_settlement(&self) -> Self {
        Self {
            prepared: self.prepared.clone(),
            row: self.row.clone(),
            guard: self.guard.clone(),
            required: self.required.clone(),
            success_audit: self.success_audit.clone(),
            post_commit: self.post_commit.clone(),
            replacing: self.replacing,
            committed_version: self.committed_version,
            failure: Arc::clone(&self.failure),
        }
    }

    pub(crate) fn committed_entity(&self) -> Entity {
        let mut entity = self.prepared.entity.clone();
        entity.version = self.committed_version;
        entity
    }

    pub(crate) async fn apply(
        &self,
        writer: &mut dyn SqlWriter,
    ) -> Result<Option<PostCommitEffect>, AtomicOpFailure> {
        if let Some(guard) = &self.guard {
            if let Some(conflict) = guard
                .check(writer)
                .await
                .map_err(|source| self.fail(FailureClass::RecordWrite, Some(source)))?
            {
                self.contention();
                return Err(AtomicOpFailure::EntityConflict(conflict));
            }
        }
        if super::faults::consume_record_write_fail(&self.prepared.entity.namespace) {
            return Err(self.fail(FailureClass::RecordWrite, None));
        }
        let affected = writer
            .execute(self.row.clone())
            .await
            .map_err(|source| self.fail(FailureClass::RecordWrite, Some(source)))?;
        if affected != 1 {
            self.contention();
            return Err(AtomicOpFailure::GuardFailed {
                statement_label: self.row.label.clone(),
                expected: AffectedRowGuard::exactly(1),
                observed: affected,
            });
        }
        // The stamp is already in the single row DML. This logical checkpoint
        // proves rollback without a second UPDATE or revision increment.
        if super::faults::consume_stamp_fail(&self.prepared.entity.namespace) {
            return Err(self.fail(FailureClass::Stamp, None));
        }
        for statement in &self.required {
            let affected = writer
                .execute(statement.statement.clone())
                .await
                .map_err(|source| self.fail(FailureClass::RecordWrite, Some(source)))?;
            if statement
                .guard
                .is_some_and(|guard| !guard.holds_for(affected))
            {
                return Err(self.fail(FailureClass::RecordWrite, None));
            }
        }
        if super::faults::consume_success_audit_fail(&self.prepared.entity.namespace) {
            return Err(self.fail(FailureClass::SuccessAudit, None));
        }
        for statement in &self.success_audit {
            let affected = writer
                .execute(statement.clone())
                .await
                .map_err(|source| self.fail(FailureClass::SuccessAudit, Some(source)))?;
            if affected != 1 {
                return Err(self.fail(FailureClass::SuccessAudit, None));
            }
        }
        Ok((self.post_commit != PostCommitEffect::None).then(|| self.post_commit.clone()))
    }

    fn contention(&self) {
        *self
            .failure
            .lock()
            .expect("entity finalizer failure lock poisoned") = Some(RecordedFailure::Contention);
    }

    fn fail(&self, class: FailureClass, source: Option<StorageError>) -> AtomicOpFailure {
        let diagnostic = self.diagnostic(class);
        *self
            .failure
            .lock()
            .expect("entity finalizer failure lock poisoned") =
            Some(RecordedFailure::Finalization {
                diagnostic,
                source: source.map(Box::new),
            });
        AtomicOpFailure::SqlError {
            statement_label: Some("entity-manifest-finalization".into()),
            message: format!("entity manifest finalization failed at {class:?}"),
        }
    }

    fn diagnostic(&self, class: FailureClass) -> FailureDiagnostic {
        FailureDiagnostic::new(
            self.prepared.entity.id,
            Substrate::Entity,
            &self.prepared.entity.namespace,
            self.prepared.entry.family(self.replacing),
            class,
        )
    }

    pub(crate) fn finish_commit(&self) {
        observe(
            &self.prepared.entity.namespace,
            FinalizerOutcome::Exempted(ExemptionCommit {
                record_id: self.prepared.entity.id,
                substrate: Substrate::Entity,
                entry_point: self.prepared.entry.family(self.replacing),
                digest_sha256: self.prepared.digest_sha256.clone(),
                field_scope: self.prepared.field_scope,
                manifest_id: self.prepared.snapshot.manifest_id().into(),
            }),
        );
    }

    /// Called only after the outer owner confirms rollback, never from the
    /// failed writer callback. Native causes survive the best-effort audit.
    pub(crate) async fn finish_rollback(&self, access: &dyn SqlAccess) -> Option<StorageError> {
        let recorded = self
            .failure
            .lock()
            .expect("entity finalizer failure lock poisoned")
            .take();
        let (diagnostic, source) = match recorded {
            Some(RecordedFailure::Contention) => return None,
            Some(RecordedFailure::Finalization { diagnostic, source }) => (diagnostic, source),
            None => (self.diagnostic(FailureClass::RecordWrite), None),
        };
        let outcome = match diagnostic.failure_class {
            FailureClass::RecordWrite => FinalizerOutcome::RecordWriteFailed(diagnostic.clone()),
            FailureClass::Stamp => FinalizerOutcome::StampFailed(diagnostic.clone()),
            FailureClass::SuccessAudit => FinalizerOutcome::AuditFailed(diagnostic.clone()),
        };
        observe(&diagnostic.namespace, outcome);
        let event = self.prepared.attribution.stamp(Event::new(
            &diagnostic.namespace, self.prepared.entry.verb(), EventKind::Audit, SubstrateKind::Entity, "",
        ).with_target(diagnostic.record_id).with_outcome(EventOutcome::Error).with_payload(serde_json::json!({
            "mechanism": "content-sha256-manifest-v1", "outcome": match diagnostic.failure_class {
                FailureClass::RecordWrite => "record-write-failed",
                FailureClass::Stamp => "stamp-failed",
                FailureClass::SuccessAudit => "audit-failed",
            },
            "diagnostic_id": diagnostic.diagnostic_id, "record_id": diagnostic.record_id,
            "entry_point": diagnostic.entry_point, "failure_class": format!("{:?}", diagnostic.failure_class),
        })));
        let failed = if super::faults::consume_failure_audit_fail(&diagnostic.namespace) {
            true
        } else {
            match event_insert_statements(&event) {
                Err(_) => true,
                Ok(statements) => access
                    .atomic_unit(Box::new(move |writer| {
                        Box::pin(async move {
                            for statement in statements {
                                if writer.execute(statement).await? != 1 {
                                    return Err(StorageError::Internal(
                                        "entity finalizer failure audit was not persisted".into(),
                                    ));
                                }
                            }
                            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
                        })
                    }))
                    .await
                    .is_err(),
            }
        };
        if failed {
            let gap = FinalizerAuditGap {
                failure_class: diagnostic.failure_class,
                diagnostic_id: diagnostic.diagnostic_id,
                record_id: diagnostic.record_id,
                substrate: diagnostic.substrate,
                namespace: diagnostic.namespace,
                entry_point: diagnostic.entry_point,
            };
            TracingLogSink.record_audit_gap(&gap);
            #[cfg(test)]
            super::entity_admission::fixture::record_gap(gap);
        }
        source.map(|source| *source)
    }
}
