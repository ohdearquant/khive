#[cfg(test)]
use super::race_seam;
use super::{
    entity_fts_document, map_merge_entity_storage_error, merge_entity_sql, merge_properties,
    stale_entity_snapshot_error, Any, AtomicUnitOp, ContentMergeStrategy, EmbeddingModelPlan,
    Entity, EntityDedupMergePolicy, EntityMergeValidation, EntityPatch, EventAttribution,
    EventKind, HashMap, KhiveRuntime, MergeEventContext, MergeSqlError, MergeSummary,
    MergeTxLimits, NamespaceToken, RuntimeError, RuntimeResult, RuntimeWriteOperation,
    SqlStatement, SqlValue, SqliteError, SubstrateKind, Uuid, Value,
};

use crate::secret_gate_finalizer::entity_admission::{
    EntityAdmission, OrdinaryEntityUpdateContext,
};
use crate::EntityCandidateMutation;

#[derive(Clone, Copy)]
enum EntityUpdateRoute<'a> {
    ReservationOnly,
    OrdinaryConstructor(&'a OrdinaryEntityUpdateContext),
}

struct PreparedEntityUpdate {
    entity: Entity,
    reindex_required: bool,
    changed_fields: Vec<&'static str>,
    expected_updated_at: i64,
    expected_deleted_at: Option<i64>,
    original: Entity,
}

impl KhiveRuntime {
    /// Patch-style entity update.
    ///
    /// Only fields set to `Some(_)` are changed. Re-indexes FTS5 (and vectors if configured)
    /// when `name`, `description`, or `entity_type` changes; skips re-indexing for
    /// property/tag-only patches.
    ///
    /// Returns `RuntimeError::NotFound` if the entity does not exist or belongs to a different
    /// namespace. Namespace isolation is enforced at the runtime layer.
    /// Computes the patched `Entity`, `reindex_required`, and `changed_fields` without
    /// writing anything, so both the normal write path and the atomic-prepare path
    /// share one source of truth for what a patched entity looks like.
    pub(crate) async fn prepare_update_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<(Entity, bool, Vec<&'static str>, i64, Option<i64>)> {
        self.prepare_guarded_entity_update(token, id, patch, None, &[])
            .await
    }

    async fn prepare_guarded_entity_update(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
        expected: Option<&Entity>,
        remove_properties: &[&str],
    ) -> RuntimeResult<(Entity, bool, Vec<&'static str>, i64, Option<i64>)> {
        let prepared = self
            .prepare_entity_update_for_route(
                token,
                id,
                patch,
                expected,
                remove_properties,
                EntityUpdateRoute::ReservationOnly,
            )
            .await?;
        Ok((
            prepared.entity,
            prepared.reindex_required,
            prepared.changed_fields,
            prepared.expected_updated_at,
            prepared.expected_deleted_at,
        ))
    }

    async fn prepare_entity_update_for_route(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
        expected: Option<&Entity>,
        remove_properties: &[&str],
        route: EntityUpdateRoute<'_>,
    ) -> RuntimeResult<PreparedEntityUpdate> {
        crate::secret_gate::reject_reserved_secret_gate_property(patch.properties.as_ref())?;
        if !remove_properties.is_empty() {
            let removals = Value::Object(
                remove_properties
                    .iter()
                    .map(|key| ((*key).to_string(), Value::Null))
                    .collect(),
            );
            crate::secret_gate::reject_reserved_secret_gate_property(Some(&removals))?;
        }
        if matches!(route, EntityUpdateRoute::ReservationOnly) {
            if let Some(ref name) = patch.name {
                crate::secret_gate::check_at(name, "entity", "name")?;
            }
            if let Some(Some(ref desc)) = patch.description {
                crate::secret_gate::check_at(desc, "entity", "description")?;
            }
            if let Some(ref props) = patch.properties {
                crate::secret_gate::check_json_at(props, "entity", "properties")?;
            }
            if let Some(ref tags) = patch.tags {
                crate::secret_gate::check_tags_at(tags, "entity", "tags")?;
            }
        } else if let EntityUpdateRoute::OrdinaryConstructor(context) = route {
            context.check_patch(
                patch.name.as_deref(),
                patch
                    .description
                    .as_ref()
                    .and_then(|value| value.as_deref()),
                patch.properties.as_ref(),
                patch.tags.as_deref(),
            )?;
        }
        let store = self.entities(token)?;
        let mut entity = store.get_entity(id).await?.ok_or_else(|| {
            if expected.is_some() {
                stale_entity_snapshot_error(id)
            } else {
                RuntimeError::NotFound(format!("entity {id}"))
            }
        })?;
        if let Some(expected) = expected {
            let actual = serde_json::to_value(&entity)
                .map_err(|error| RuntimeError::Internal(error.to_string()))?;
            let expected = serde_json::to_value(expected)
                .map_err(|error| RuntimeError::Internal(error.to_string()))?;
            if actual != expected {
                return Err(stale_entity_snapshot_error(id));
            }
        }
        let original = entity.clone();
        let expected_updated_at = entity.updated_at;
        let expected_deleted_at = entity.deleted_at;
        #[cfg(test)]
        race_seam::pause_after_read().await;

        // ADR-014 tri-state: outer `None` = unchanged; `Some(None)` = explicit
        // clear (no vocabulary validation — there is no value to validate);
        // `Some(Some(raw))` = set, validated and normalized.
        let validated_entity_type = match &patch.entity_type {
            Some(None) => Some(None),
            Some(Some(raw)) => Some(Some(
                self.validate_entity_type_for_kind(&entity.kind, Some(raw))?
                    .expect("set branch always yields a normalized value"),
            )),
            None => None,
        };

        let mut reindex_required = false;
        let mut changed_fields: Vec<&'static str> = Vec::new();

        if let Some(name) = patch.name {
            reindex_required |= entity.name != name;
            entity.name = name;
            changed_fields.push("name");
        }
        if let Some(desc_patch) = patch.description {
            reindex_required |= entity.description != desc_patch;
            entity.description = desc_patch;
            changed_fields.push("description");
        }
        if let Some(props) = patch.properties {
            let (merged, _) = merge_properties(
                &entity.properties,
                &Some(props),
                EntityDedupMergePolicy::PreferFrom,
            );
            entity.properties = merged;
            changed_fields.push("properties");
        }
        if let Some(Value::Object(properties)) = entity.properties.as_mut() {
            let mut removed = false;
            for key in remove_properties {
                removed |= properties.remove(*key).is_some();
            }
            if removed && !changed_fields.contains(&"properties") {
                changed_fields.push("properties");
            }
        }
        if let Some(tags) = patch.tags {
            entity.tags = tags;
            changed_fields.push("tags");
        }
        if let Some(entity_type) = validated_entity_type {
            reindex_required |= entity.entity_type != entity_type;
            entity.entity_type = entity_type;
            changed_fields.push("entity_type");
        }

        // A patch may carry properties from the stored row into the full
        // replacement. Validate the final object, including that carry.
        crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;

        if expected.is_some() && changed_fields.is_empty() {
            return Ok(PreparedEntityUpdate {
                entity,
                reindex_required,
                changed_fields,
                expected_updated_at,
                expected_deleted_at,
                original,
            });
        }

        // #2943: `entity.properties`, `entity.entity_type`, and `entity.tags`
        // are all final here — the owning pack's KindHook, if any, validates
        // the resulting record, mirroring `prepare_note_update_hook` on the
        // note side. `Ok(())` when no pack registered a hook for this entity
        // kind (the trait default, or the runtime-layer aggregate was never
        // installed). Placed after the no-op early return above so a
        // genuinely unchanged guarded update never re-runs the hook for
        // nothing; only `updated_at` remains to be bumped after this point.
        if let Some(hook) = self.entity_kind_hook(&entity.kind) {
            hook.validate_entity_update(self, token, &entity, entity.properties.as_ref())
                .await?;
        }

        // `updated_at` is also the optimistic-concurrency revision for
        // full-entity replacement. Make it strictly advance even when two
        // operations land inside one clock microsecond. Saturation is not a
        // valid fallback: reusing i64::MAX would make the CAS accept a write
        // without advancing its revision.
        let minimum_updated_at = expected_updated_at.checked_add(1).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "entity {id} updated_at is already at i64::MAX and cannot advance"
            ))
        })?;
        entity.updated_at = chrono::Utc::now()
            .timestamp_micros()
            .max(minimum_updated_at);
        Ok(PreparedEntityUpdate {
            entity,
            reindex_required,
            changed_fields,
            expected_updated_at,
            expected_deleted_at,
            original,
        })
    }

    #[cfg(test)]
    pub(crate) async fn update_entity(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<Entity> {
        Ok(self
            .update_entity_with_embedding_report(token, id, patch)
            .await?
            .0)
    }

    pub async fn update_entity_with_embedding_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        self.update_entity_with_expected_version_and_embedding_report(token, id, patch, None)
            .await
    }

    /// Entity update with an optional caller revision, checked inside the writer transaction.
    pub async fn update_entity_with_expected_version_and_embedding_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: EntityPatch,
        expected_version: Option<i64>,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        crate::entity_write::validate_expected_version(expected_version)?;
        let admission = OrdinaryEntityUpdateContext::capture(token);
        let PreparedEntityUpdate {
            entity,
            reindex_required,
            changed_fields,
            expected_updated_at,
            expected_deleted_at,
            original,
        } = self
            .prepare_entity_update_for_route(
                token,
                id,
                patch,
                None,
                &[],
                EntityUpdateRoute::OrdinaryConstructor(&admission),
            )
            .await?;
        let entity = match admission.admit(token, entity)? {
            EntityAdmission::Legacy(entity) => entity,
            EntityAdmission::Exempt(prepared) => {
                use crate::atomic_plan::PostCommitEffect;
                use crate::atomic_runner::{
                    run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome,
                };
                let mut entity = prepared.entity().clone();
                let next_version = original
                    .version
                    .checked_add(1)
                    .ok_or_else(|| RuntimeError::InvalidInput("entity version overflow".into()))?;
                let (mut required, report) = self
                    .prepare_admitted_entity_indexes(token, &entity, &[], reindex_required)
                    .await?;
                required.extend(crate::atomic_prepare::event_append_statements(
                    token, &entity.namespace, "update", EventKind::EntityUpdated,
                    SubstrateKind::Entity, entity.id,
                    serde_json::json!({"id": entity.id, "namespace": entity.namespace, "changed_fields": changed_fields}),
                )?);
                let plan = prepared.into_plan(
                    EntityCandidateMutation::ReplaceIfUnchanged {
                        expected: original,
                        expected_version,
                    },
                    required,
                    PostCommitEffect::None,
                )?;
                match run_atomic_unit(
                    self.sql().as_ref(),
                    vec![AtomicOpPlan::FinalizeEntity(Box::new(plan))],
                )
                .await
                {
                    Ok(AtomicRunOutcome::Committed { .. }) => {
                        entity.version = next_version;
                        return Ok((entity, report));
                    }
                    Ok(AtomicRunOutcome::RolledBack {
                        failure: AtomicOpFailure::EntityConflict(conflict),
                        ..
                    }) => {
                        return Err(conflict.into_error().into());
                    }
                    Ok(AtomicRunOutcome::RolledBack {
                        failure: AtomicOpFailure::GuardFailed { .. },
                        ..
                    }) => {
                        return Err(stale_entity_snapshot_error(id));
                    }
                    Ok(AtomicRunOutcome::RolledBack { failure, .. }) => {
                        return Err(RuntimeError::Internal(format!(
                            "entity update finalization rolled back: {failure:?}"
                        )));
                    }
                    Err(error) => return Err(RuntimeError::Storage(error.0)),
                }
            }
        };

        self.persist_prepared_entity_update(
            token,
            entity,
            reindex_required,
            changed_fields,
            expected_updated_at,
            expected_deleted_at,
            expected_version,
        )
        .await
    }

    /// Apply an admin patch only if the entity still matches the full read snapshot.
    /// Property removals apply after the normal merge and preserve all other keys.
    /// Missing keys alone are a no-op; reserved runtime-owned keys cannot be removed.
    /// A changed, deleted, or missing entity returns a conflict without writing.
    /// A bounded embedding returns a non-retryable error with the committed ID
    /// and truncation report; use the report-aware variant to retain the record.
    pub async fn update_entity_if_unchanged(
        &self,
        token: &NamespaceToken,
        expected: &Entity,
        patch: EntityPatch,
        remove_properties: &[&str],
    ) -> RuntimeResult<Entity> {
        let (entity, embedding) = self
            .update_entity_if_unchanged_with_embedding_report(
                token,
                expected,
                patch,
                remove_properties,
            )
            .await?;
        crate::operations::legacy_post_commit_result_with_embedding(
            "update_entity_if_unchanged",
            entity.id,
            entity,
            embedding,
            Vec::new(),
        )
    }

    /// Apply a guarded admin patch and retain embedding truncation accounting.
    pub async fn update_entity_if_unchanged_with_embedding_report(
        &self,
        token: &NamespaceToken,
        expected: &Entity,
        patch: EntityPatch,
        remove_properties: &[&str],
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        let (entity, reindex_required, changed_fields, expected_updated_at, expected_deleted_at) =
            self.prepare_guarded_entity_update(
                token,
                expected.id,
                patch,
                Some(expected),
                remove_properties,
            )
            .await?;
        if changed_fields.is_empty() {
            return Ok((
                entity,
                crate::retrieval::EmbeddingTruncationReport::default(),
            ));
        }
        self.persist_prepared_entity_update(
            token,
            entity,
            reindex_required,
            changed_fields,
            expected_updated_at,
            expected_deleted_at,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_prepared_entity_update(
        &self,
        token: &NamespaceToken,
        mut entity: Entity,
        reindex_required: bool,
        changed_fields: Vec<&'static str>,
        expected_updated_at: i64,
        expected_deleted_at: Option<i64>,
        expected_version: Option<i64>,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        // This final whole-object replacement must reserve the complete
        // candidate, even if a future caller bypasses the patch preparer.
        crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
        let id = entity.id;
        let _ = self.entities(token)?;
        let next_version = entity
            .version
            .checked_add(1)
            .ok_or_else(|| RuntimeError::InvalidInput("entity version overflow".into()))?;
        use crate::atomic_plan::{AffectedRowGuard, PlanStatement, PostCommitEffect, UpdatePlan};
        use crate::atomic_runner::{
            run_atomic_unit, AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome,
        };
        let plan = UpdatePlan {
            target_id: id,
            statements: vec![PlanStatement {
                statement: khive_db::stores::entity::entity_replace_if_unchanged_statement(
                    &entity,
                    expected_updated_at,
                    expected_deleted_at,
                ),
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
            post_commit: PostCommitEffect::None,
            edge_natural_key: None,
            idempotent_noop: false,
            entity_guard: expected_version.map(|expected_version| {
                crate::entity_write::EntityWriteGuard {
                    id,
                    expected_version,
                }
            }),
            note_guard: None,
            note_vector_purge: None,
            note_embedding_inheritance: None,
            graph_effects: Vec::new(),
        };
        match run_atomic_unit(
            self.sql().as_ref(),
            vec![AtomicOpPlan::Update(Box::new(plan))],
        )
        .await
        {
            Ok(AtomicRunOutcome::Committed { .. }) => entity.version = next_version,
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::EntityConflict(conflict),
                ..
            }) => return Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::GuardFailed { .. },
                ..
            }) => return Err(stale_entity_snapshot_error(id)),
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => {
                return Err(RuntimeError::Internal(format!(
                    "entity update rolled back: {failure:?}"
                )))
            }
            Err(error) => return Err(RuntimeError::Storage(error.0)),
        }

        let event_token =
            token.with_namespace(crate::Namespace::parse(&entity.namespace).map_err(|error| {
                RuntimeError::Internal(format!("entity namespace invalid: {error}"))
            })?);
        let event_store = self.events(&event_token)?;
        let event = khive_storage::event::Event::new(
            entity.namespace.clone(),
            "update",
            EventKind::EntityUpdated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(entity.id)
        .with_payload(serde_json::json!({
            "id": entity.id,
            "namespace": entity.namespace,
            "changed_fields": changed_fields,
        }));
        let event_result = event_store.append_event(event).await.map_err(|e| {
            RuntimeError::Internal(format!("update_entity: event store write failed: {e}"))
        });

        let embedding_report = if reindex_required {
            self.reindex_entity(token, &entity).await?
        } else {
            crate::retrieval::EmbeddingTruncationReport::default()
        };

        event_result?;

        Ok((entity, embedding_report))
    }

    /// Merge `from_id` into `into_id`.
    ///
    /// All edges incident to `from_id` are rewired to `into_id`. Self-loops that would
    /// result from the rewire are dropped. Properties and tags are merged per `strategy`.
    /// `from_id` is tombstoned with merge provenance and removed from indexes. Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating any rows.
    ///
    /// Atomic: all SQL (entity reads/writes, edge rewires, FTS updates, vec-index
    /// delete, merge event with destructive edge preimages) runs on one pool
    /// connection inside one `BEGIN IMMEDIATE` transaction via
    /// `merge_entity_sql`. If embedding vectors are configured, the vector re-insert for
    /// `into_id` is performed after the transaction (requires async embedding computation).
    pub async fn merge_entity(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_entity_with_reason(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            None,
        )
        .await
    }

    /// Merge `from_id` into `into_id` and include an optional reason in the audit event.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_entity_with_reason(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_entity_with_validation(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            reason,
            EntityMergeValidation::LegacyKind,
        )
        .await
    }

    /// Merge two entities with an explicit override for the entity safety floor.
    ///
    /// Non-forced calls enforce entity kind, name similarity, and project compatibility
    /// against the rows reread inside the merge transaction. Legacy merge methods retain
    /// their historical same-kind-only policy.
    /// A non-dry-run override is recorded as `force: true` in the merge event.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, audit-reason, and force fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_entity_with_reason_and_force(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
        force: bool,
    ) -> RuntimeResult<MergeSummary> {
        let validation = if force {
            EntityMergeValidation::Forced
        } else {
            EntityMergeValidation::SafetyFloor
        };
        self.merge_entity_with_validation(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            reason,
            validation,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn merge_entity_with_validation(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
        validation: EntityMergeValidation,
    ) -> RuntimeResult<MergeSummary> {
        if let Some(reason) = reason.as_deref() {
            crate::secret_gate::check_at(reason, "merge", "reason")?;
        }
        if into_id == from_id {
            return Err(RuntimeError::InvalidInput(
                "cannot merge an entity into itself".into(),
            ));
        }
        let ns = token.namespace().as_str().to_owned();
        let fts_table = "fts_entities".to_string();
        // One immutable registry view governs transactional deletion, table
        // preparation, and survivor reindex. A late model belongs to a later
        // write/backfill rather than only one leg of this merge.
        let embedding_plan = EmbeddingModelPlan::capture(self);
        let vec_tables = embedding_plan.vector_tables();
        // Loaded once here (sync, cheap) so the rewire loop can evaluate the
        // endpoint contract without an async round-trip per edge (khive#1216).
        let pack_rules = self.pack_edge_rules();

        // Ensure all required tables exist (idempotent DDL) before the transaction.
        let _ = self.entities(token)?;
        let _ = self.graph(token)?;
        let _ = self.text(token)?;
        let _ = self.events(token)?;
        // vectors_for_model (not the default-model-only self.vectors()) so
        // custom-only runtimes (no default embedding_model) still get DDL primed.
        for model_name in embedding_plan.model_names() {
            let _ = self.vectors_for_model(token, model_name)?;
        }

        let pool = self.backend().pool_arc();
        let writer_task = pool
            .writer_task_for_runtime_write(RuntimeWriteOperation::MergeEntity)
            .map_err(RuntimeError::Storage)?;
        // Minted before the transaction so the tombstone and its in-transaction
        // EntityMerged event carry the same id.
        let merge_event_id = Uuid::new_v4();
        let event_context = MergeEventContext {
            attribution: EventAttribution::from_token(token),
            reason,
            force: validation == EntityMergeValidation::Forced,
            strategy,
            content_strategy,
            kind: EventKind::EntityMerged,
            substrate: SubstrateKind::Entity,
            event_id: Some(merge_event_id),
        };

        let (mut summary, updated_entity) = if let Some(writer_task) = writer_task {
            writer_task
                .send(move |conn| {
                    merge_entity_sql(
                        conn,
                        ns,
                        fts_table,
                        vec_tables,
                        into_id,
                        from_id,
                        strategy,
                        content_strategy,
                        dry_run,
                        pack_rules,
                        validation,
                        MergeTxLimits::default(),
                        merge_event_id,
                        Some(event_context),
                    )
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Entities,
                            "merge_entity",
                            e,
                        )
                    })
                })
                .await
                .inspect_err(|error| khive_storage::usage::account_event_write(Err(error)))
                .map_err(map_merge_entity_storage_error)?
        } else {
            tokio::task::spawn_blocking(move || {
                let guard = pool.writer()?;
                let mut refusal = None;
                let result = guard.transaction(|conn| {
                    merge_entity_sql(
                        conn,
                        ns,
                        fts_table,
                        vec_tables,
                        into_id,
                        from_id,
                        strategy,
                        content_strategy,
                        dry_run,
                        pack_rules,
                        validation,
                        MergeTxLimits::default(),
                        merge_event_id,
                        Some(event_context),
                    )
                    .map_err(|error| match error {
                        MergeSqlError::Sqlite(error) => error,
                        MergeSqlError::Refusal(error) => {
                            refusal = Some(error);
                            SqliteError::InvalidData(
                                "entity merge refused by transactional policy".to_string(),
                            )
                        }
                    })
                });
                match refusal {
                    Some(error) => Err(error),
                    None => result.map_err(RuntimeError::from),
                }
            })
            .await
            .map_err(|e| RuntimeError::Internal(e.to_string()))??
        };

        // Count only committed event rows; dry-run never inserts an event.
        if !dry_run {
            khive_storage::usage::account_event_write(Ok(1));
            tracing::info!(
                into_id = %summary.kept_id,
                from_id = %summary.removed_id,
                budget_rows = summary.tx_budget.rows_charged,
                budget_bytes = summary.tx_budget.bytes_charged,
                budget_max_rows = summary.tx_budget.max_rows,
                budget_max_bytes = summary.tx_budget.max_bytes,
                "merge_entity: transaction materialization budget"
            );
        }

        // FTS and vec-deletes already committed inside the transaction above;
        // only the embedding re-insert needs an async step outside it.
        if !dry_run && !embedding_plan.is_empty() {
            match self
                .reindex_entity_with_plan(token, &updated_entity, &embedding_plan, None)
                .await
            {
                Ok(report) => summary.embedding_truncation = report,
                Err(error) => {
                    tracing::warn!(
                        into_id = %summary.kept_id,
                        from_id = %summary.removed_id,
                        error = %error,
                        "merge_entity: committed merge but survivor reindex failed"
                    );
                    summary.post_commit_reindex_error = Some(error.to_string());
                }
            }
        }

        Ok(summary)
    }

    // ---- Internal helpers ----

    async fn apply_entity_index_revision(
        &self,
        entity: &Entity,
        statements: Vec<SqlStatement>,
    ) -> RuntimeResult<bool> {
        let namespace = entity.namespace.clone();
        let id = entity.id.to_string();
        let version = entity.version;
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let current = writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT version FROM entities \
                              WHERE namespace=?1 AND id=?2 AND deleted_at IS NULL"
                            .into(),
                        params: vec![SqlValue::Text(namespace), SqlValue::Text(id)],
                        label: Some("entity-index-revision".into()),
                    })
                    .await?;
                if !matches!(current, Some(SqlValue::Integer(current)) if current == version) {
                    return Ok(Box::new(false) as Box<dyn Any + Send>);
                }
                for statement in statements {
                    writer.execute(statement).await?;
                }
                Ok(Box::new(true) as Box<dyn Any + Send>)
            })
        });
        self.sql()
            .atomic_unit(op)
            .await?
            .downcast::<bool>()
            .map(|result| *result)
            .map_err(|_| RuntimeError::Internal("invalid entity index outcome".into()))
    }

    pub(crate) fn entity_vector_insert_statements(
        table: &str,
        entity: &Entity,
        model_name: &str,
        vector: &[f32],
    ) -> Vec<SqlStatement> {
        let subject = entity.id.to_string();
        let model_key = table
            .strip_prefix("vec_")
            .expect("runtime vector tables use the vec_ prefix");
        let kind = SubstrateKind::Entity.to_string();
        let field = "entity.body";
        let blob = khive_storage::encode_f32_native(vector);
        vec![
            SqlStatement {
                sql: format!(
                    "INSERT INTO ann_write_log \
                     (namespace, embedding_model, kind, field, subject_id, op) \
                     SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                     FROM {table} WHERE subject_id=?1 AND NOT \
                     (namespace=?2 AND embedding_model=?3 AND kind=?4 AND field=?5)"
                ),
                params: vec![
                    SqlValue::Text(subject.clone()),
                    SqlValue::Text(entity.namespace.clone()),
                    SqlValue::Text(model_name.to_string()),
                    SqlValue::Text(kind.clone()),
                    SqlValue::Text(field.into()),
                ],
                label: Some("entity-reindex-log-delete".into()),
            },
            SqlStatement {
                sql: format!("DELETE FROM {table} WHERE subject_id=?1"),
                params: vec![SqlValue::Text(subject.clone())],
                label: Some("entity-reindex-vector-delete".into()),
            },
            // This raw replacement cannot attest the embedded input. Clear the old
            // sidecar in the same atomic index revision even when the new BLOB is
            // byte-identical to the old one.
            SqlStatement {
                sql: "DELETE FROM vector_provenance \
                      WHERE model_key = ?1 AND subject_id = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text(model_key.to_string()),
                    SqlValue::Text(subject.clone()),
                ],
                label: Some("entity-reindex-provenance-clear".into()),
            },
            SqlStatement {
                sql: format!(
                    "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
                ),
                params: vec![
                    SqlValue::Text(subject.clone()),
                    SqlValue::Text(entity.namespace.clone()),
                    SqlValue::Text(kind.clone()),
                    SqlValue::Text(field.into()),
                    SqlValue::Text(model_name.to_string()),
                    SqlValue::Blob(blob),
                ],
                label: Some("entity-reindex-vector-insert".into()),
            },
            SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Text(entity.namespace.clone()),
                    SqlValue::Text(model_name.to_string()),
                    SqlValue::Text(kind),
                    SqlValue::Text(field.into()),
                    SqlValue::Text(subject),
                ],
                label: Some("entity-reindex-log-upsert".into()),
            },
        ]
    }

    pub(crate) async fn publish_entity_vector_revision(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        model_name: &str,
        vector: &[f32],
    ) -> RuntimeResult<bool> {
        self.vectors_for_model(token, model_name)?;
        let (storage_model, dimensions) = self.vector_model_metadata(model_name)?;
        if let Some(index) = vector.iter().position(|value| !value.is_finite()) {
            return Err(RuntimeError::InvalidInput(format!(
                "non-finite entity vector at index {index}"
            )));
        }
        if vector.len() != dimensions {
            return Err(RuntimeError::InvalidInput(format!(
                "entity vector has {} dimensions; expected {dimensions}",
                vector.len()
            )));
        }
        let table = format!("vec_{}", crate::config::sanitize_key(&storage_model));
        let statements =
            Self::entity_vector_insert_statements(&table, entity, &storage_model, vector);
        #[cfg(test)]
        race_seam::pause_before_entity_vector_publish().await;
        self.apply_entity_index_revision(entity, statements).await
    }

    /// Re-upsert FTS5 document and vector(s) for the entity across all registered models.
    ///
    /// Uses `entity.namespace` — the authoritative namespace stored on the record — rather
    /// than the caller-supplied `namespace` parameter. This prevents a cross-namespace
    /// reindex from writing the search document into the wrong namespace's FTS index.
    ///
    /// Best-effort for vectors: if embedding or inserting for a particular model fails,
    /// logs a warning and continues to the next model. The FTS step is fail-closed
    /// (propagates error). Callers (update_entity, merge_entity) have already committed
    /// the entity row, so a partial embed miss leaves a stale vector rather than
    /// rolling back the update. Each failed guarded vector replacement rolls back
    /// its own index writes, keeping the prior row searchable.
    pub(crate) async fn reindex_entity(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        let embedding_plan = EmbeddingModelPlan::capture(self);
        self.reindex_entity_with_plan(token, entity, &embedding_plan, None)
            .await
    }

    pub(crate) async fn reindex_entity_with_precomputed(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        mut precomputed: HashMap<String, crate::retrieval::DocumentEmbeddingOutcome>,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        let embedding_plan = EmbeddingModelPlan::capture(self);
        self.reindex_entity_with_plan(token, entity, &embedding_plan, Some(&mut precomputed))
            .await
    }

    pub(super) async fn reindex_entity_with_plan(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
        embedding_plan: &EmbeddingModelPlan,
        mut precomputed: Option<&mut HashMap<String, crate::retrieval::DocumentEmbeddingOutcome>>,
    ) -> RuntimeResult<crate::retrieval::EmbeddingTruncationReport> {
        // Test-only fault seam: force the post-commit FTS leg to fail after a
        // merge or update has already persisted its entity row.
        #[cfg(test)]
        if crate::operations::consume_fts_fail_fault(&entity.namespace) {
            return Err(RuntimeError::Internal("injected FTS failure".to_string()));
        }
        // Use entity.namespace (authoritative) rather than token.namespace().as_str() (caller claim).
        let doc = entity_fts_document(entity);
        let embed_body = doc.body.clone();
        let _ = self.text(token)?;
        #[cfg(test)]
        race_seam::pause_before_entity_index_publish().await;
        let statements = khive_db::stores::text::delete_document_statements(
            "fts_entities",
            &entity.namespace,
            entity.id,
        )
        .into_iter()
        .chain(khive_db::stores::text::insert_document_statements(
            "fts_entities",
            &doc,
        ))
        .collect();
        if !self.apply_entity_index_revision(entity, statements).await? {
            return Ok(crate::retrieval::EmbeddingTruncationReport::default());
        }

        let mut report = crate::retrieval::EmbeddingTruncationReport::default();
        for model_name in embedding_plan.model_names() {
            let embedding = match precomputed
                .as_mut()
                .and_then(|outcomes| outcomes.remove(model_name))
            {
                Some(outcome) => Ok(outcome),
                None => {
                    self.embed_document_with_model_outcome_for_token(token, model_name, &embed_body)
                        .await
                }
            };
            match embedding {
                Ok(outcome) => {
                    report.observe(&outcome);
                    match self
                        .publish_entity_vector_revision(token, entity, model_name, &outcome.vector)
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(error) => tracing::warn!(
                            model = model_name,
                            id = %entity.id,
                            "reindex_entity: vector insert failed, skipping model: {error}"
                        ),
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        model = model_name,
                        id = %entity.id,
                        "reindex_entity: embed failed for model, skipping: {e}"
                    );
                }
            }
        }

        Ok(report)
    }
}
