//! Direct-ingest facade retaining preflight snapshot and conditional writes.

use khive_storage::Entity;

use super::entity_admission::{EntityAdmission, EntityCandidatePrepared};
use super::entity_transaction::EntityCandidateMutation;
use crate::{
    AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome, KhiveRuntime, NamespaceToken,
    PostCommitEffect, RuntimeError, RuntimeResult,
};

#[derive(Debug)]
pub enum EntityCandidateAdmission {
    /// No write occurred. Continue the existing clean-candidate storage route.
    Legacy(Entity),
    /// Record, runtime stamp, required FTS/map, and audit committed together.
    Committed(Entity),
    /// Conditional insert/CAS lost; the ingest owner may reread and rebase.
    Conflict,
}

impl KhiveRuntime {
    /// Admit a structural direct-ingest candidate. Matched candidates cannot
    /// carry content_ref, because this facade does not own attachment writes.
    /// The ordinary attachment-aware constructor remains the supported path
    /// for those records; clean Legacy candidates keep their original shape.
    pub async fn try_commit_manifest_entity_candidate(
        &self,
        token: &NamespaceToken,
        prepared: EntityCandidatePrepared,
        mutation: EntityCandidateMutation,
    ) -> RuntimeResult<EntityCandidateAdmission> {
        let admitted = match prepared.admit(token)? {
            EntityAdmission::Legacy(entity) => return Ok(EntityCandidateAdmission::Legacy(entity)),
            EntityAdmission::Exempt(admitted) => admitted,
        };
        self.validate_entity_kind(&admitted.entity().kind)?;
        self.validate_entity_type_for_kind(
            &admitted.entity().kind,
            admitted.entity().entity_type.as_deref(),
        )?;
        let (required, _) = self
            .prepare_admitted_entity_indexes(token, admitted.entity(), &[], false)
            .await?;
        let plan = admitted.into_plan(mutation, required, PostCommitEffect::None)?;
        let committed = plan.committed_entity();
        match crate::run_atomic_unit(
            self.sql().as_ref(),
            vec![AtomicOpPlan::FinalizeEntity(Box::new(plan))],
        )
        .await
        {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                Ok(EntityCandidateAdmission::Committed(committed))
            }
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::EntityConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::GuardFailed { .. },
                ..
            }) => Ok(EntityCandidateAdmission::Conflict),
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("entity manifest finalization rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }
}
