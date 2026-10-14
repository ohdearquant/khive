//! Prepared entity bulk construction.

use super::*;
use crate::secret_gate_finalizer::entity_admission::{
    prepare_entity_admission_at, EntityAdmission, EntityEntryPoint,
};
use crate::EntityCandidateMutation;

impl KhiveRuntime {
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
        let mut admissions = Vec::with_capacity(specs.len());
        for (index, spec) in specs.iter().enumerate() {
            let entity = self.validate_bulk_entity(ns, spec)?;
            admissions.push(prepare_entity_admission_at(
                token,
                EntityEntryPoint::Bulk,
                entity,
                &format!("entity[{index}]"),
            )?);
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
            Some(usize::from(admissions.len() > 1))
        } else {
            None
        };

        let _ = self.entities(token)?;
        let _ = self.text(token)?;

        let mut entities = Vec::with_capacity(admissions.len());
        let mut plans = Vec::with_capacity(admissions.len());
        let mut has_finalizer = false;
        for (index, admission) in admissions.into_iter().enumerate() {
            has_finalizer |= matches!(&admission, EntityAdmission::Exempt(_));
            let (entity, plan) = self
                .finish_bulk_entity_plan(token, admission, injected_failure_index == Some(index))
                .await?;
            entities.push(entity);
            plans.push(plan);
        }

        match run_atomic_unit(self.sql().as_ref(), plans).await {
            Ok(AtomicRunOutcome::Committed { .. }) => Ok(entities),
            Ok(AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            }) => Err(RuntimeError::Internal(format!(
                "create_many: atomic batch rolled back at entity index {failed_op_index}: \
                 {failure:?}"
            ))),
            Err(e) if has_finalizer => Err(RuntimeError::Storage(e.0)),
            Err(e) => Err(RuntimeError::Internal(format!(
                "create_many: atomic batch failed: {}",
                e.0
            ))),
        }
    }

    /// One bulk entity spec's pre-write checks and its row, shared by
    /// `create_many` and [`Self::prepare_bulk_entity_plan`] so the two bulk
    /// entity paths cannot drift apart: kind, entity_type, a nonempty name,
    /// the reserved secret-gate property and the secret gate itself.
    fn validate_bulk_entity(&self, ns: &str, spec: &EntityCreateSpec) -> RuntimeResult<Entity> {
        self.validate_entity_kind(&spec.kind)?;
        // Validate entity_type at the runtime layer via pack-installed callback.
        // When no validator is installed (bare runtime, unit tests without packs),
        // the type passes through unchanged, the same skip-when-absent pattern as
        // validate_entity_kind. The handler layer remains the primary enforcement point.
        let validated_type =
            self.validate_entity_type_for_kind(&spec.kind, spec.entity_type.as_deref())?;
        if spec.name.trim().is_empty() {
            return Err(RuntimeError::InvalidInput("name must not be empty".into()));
        }
        crate::secret_gate::reject_reserved_secret_gate_property(spec.properties.as_ref())?;

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
        Ok(entity)
    }

    /// Validate and prepare one entity item for a bulk `create(items=[...])`
    /// write: the same admission and row/FTS plan as [`Self::create_many`],
    /// with no scheduled reindex, so the vector is deferred to a later
    /// `reindex` exactly as for `create_many`. The bulk create handler uses
    /// this for every entity item so entity and note plans can join one
    /// `run_atomic_unit` call.
    pub async fn prepare_bulk_entity_plan(
        &self,
        token: &NamespaceToken,
        spec: EntityCreateSpec,
    ) -> RuntimeResult<(Entity, AtomicOpPlan)> {
        let entity = self.validate_bulk_entity(token.namespace().as_str(), &spec)?;
        let admission =
            prepare_entity_admission_at(token, EntityEntryPoint::Bulk, entity, "entity")?;
        let _ = self.entities(token)?;
        let _ = self.text(token)?;
        self.finish_bulk_entity_plan(token, admission, false).await
    }

    async fn finish_bulk_entity_plan(
        &self,
        token: &NamespaceToken,
        admission: EntityAdmission,
        inject_fts_failure: bool,
    ) -> RuntimeResult<(Entity, AtomicOpPlan)> {
        match admission {
            EntityAdmission::Legacy(entity) => {
                let mut plan = bulk_entity_plan(&entity)?;
                if inject_fts_failure {
                    plan.statements.truncate(1);
                    plan.statements.push(bulk_fts_failure());
                }
                Ok((entity, AtomicOpPlan::AddEntity(plan)))
            }
            EntityAdmission::Exempt(prepared) => {
                let entity = prepared.entity().clone();
                let (mut required, _) = self
                    .prepare_admitted_entity_indexes(token, &entity, &[], false)
                    .await?;
                if inject_fts_failure {
                    required = vec![bulk_fts_failure()];
                }
                let plan = prepared.into_plan(
                    EntityCandidateMutation::CreateIfAbsent,
                    required,
                    PostCommitEffect::None,
                )?;
                Ok((entity, AtomicOpPlan::FinalizeEntity(Box::new(plan))))
            }
        }
    }
}

fn bulk_entity_plan(entity: &Entity) -> RuntimeResult<AddEntityPlan> {
    crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
    let mut statements = vec![PlanStatement {
        statement: entity_upsert_statement(entity),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    // The FTS insert and rowid-map insert must remain adjacent on one connection.
    statements.extend(
        insert_document_statements("fts_entities", &entity_fts_document(entity))
            .into_iter()
            .map(|statement| PlanStatement {
                statement,
                guard: None,
            }),
    );
    Ok(AddEntityPlan {
        entity_id: entity.id,
        statements,
        post_commit: PostCommitEffect::None,
    })
}

fn bulk_fts_failure() -> PlanStatement {
    PlanStatement {
        statement: SqlStatement {
            sql: "INSERT INTO __khive_create_many_injected_failure__ DEFAULT VALUES".into(),
            params: vec![],
            label: Some("fts-insert-injected-failure".into()),
        },
        guard: None,
    }
}
