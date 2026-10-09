//! Entity construction, claims and their indexing outcomes.

use super::*;
use crate::secret_gate_finalizer::entity_admission::{
    prepare_entity_admission, EntityAdmission, EntityEntryPoint,
};
use crate::EntityCandidateMutation;

fn claimed_entity_create_event(entity: &Entity) -> Event {
    let mut event = Event::new(
        entity.namespace.clone(),
        "create",
        EventKind::EntityCreated,
        SubstrateKind::Entity,
        "",
    )
    .with_target(entity.id)
    .with_payload(serde_json::json!({
        "id": entity.id,
        "namespace": &entity.namespace,
        "kind": &entity.kind,
    }));
    let event_seed = Uuid::new_v5(&Uuid::NAMESPACE_URL, b"khive:claimed-entity-create:v1");
    let mut event_key = Vec::with_capacity(24);
    event_key.extend_from_slice(entity.id.as_bytes());
    event_key.extend_from_slice(&entity.created_at.to_be_bytes());
    event.id = Uuid::new_v5(&event_seed, &event_key);
    event
}

impl KhiveRuntime {
    /// Claim a caller-derived entity id without replacing a competing row.
    /// Index writes are repeatable and never compensate by deleting the claim.
    pub async fn claim_entity_if_absent(
        &self,
        token: &NamespaceToken,
        spec: EntityClaimSpec,
    ) -> RuntimeResult<(Entity, bool)> {
        self.validate_entity_kind(&spec.kind)?;
        let entity_type =
            self.validate_entity_type_for_kind(&spec.kind, spec.entity_type.as_deref())?;
        crate::secret_gate::reject_reserved_secret_gate_property(spec.properties.as_ref())?;

        let mut proposed = Entity::new(token.namespace().as_str(), &spec.kind, &spec.name);
        proposed.id = spec.id;
        proposed.entity_type = entity_type.clone();
        proposed.description = spec.description;
        proposed.properties = spec.properties;
        proposed.tags = spec.tags;

        let admission = prepare_entity_admission(token, EntityEntryPoint::Claim, proposed)?;
        let store = self.entities(token)?;
        let (inserted, indexes_committed) = match admission {
            EntityAdmission::Legacy(candidate) => {
                proposed = candidate;
                (
                    store.insert_entity_if_absent(proposed.clone()).await?,
                    false,
                )
            }
            EntityAdmission::Exempt(prepared) => {
                proposed = prepared.entity().clone();
                let (mut required, _) = self
                    .prepare_admitted_entity_indexes(token, &proposed, &[], true)
                    .await?;
                let _ = self.events(token)?;
                let event = crate::EventAttribution::from_token(token)
                    .stamp(claimed_entity_create_event(&proposed));
                required.extend(
                    khive_db::stores::event::event_insert_statements(&event)
                        .map_err(|error| RuntimeError::Internal(error.to_string()))?
                        .into_iter()
                        .map(|statement| PlanStatement {
                            statement,
                            guard: Some(AffectedRowGuard::exactly(1)),
                        }),
                );
                let plan = prepared.into_plan(
                    EntityCandidateMutation::CreateIfAbsent,
                    required,
                    PostCommitEffect::None,
                )?;
                match run_atomic_unit(
                    self.sql().as_ref(),
                    vec![AtomicOpPlan::FinalizeEntity(Box::new(plan))],
                )
                .await
                {
                    Ok(AtomicRunOutcome::Committed { .. }) => (true, true),
                    Ok(AtomicRunOutcome::RolledBack {
                        failure: crate::atomic_runner::AtomicOpFailure::GuardFailed { .. },
                        ..
                    }) => (false, false),
                    Ok(AtomicRunOutcome::RolledBack { failure, .. }) => {
                        return Err(RuntimeError::Internal(format!(
                            "entity claim finalization rolled back: {failure:?}"
                        )));
                    }
                    Err(error) => return Err(RuntimeError::Storage(error.0)),
                }
            }
        };
        let entity = if inserted {
            proposed
        } else {
            store
                .get_entity_including_deleted(spec.id)
                .await?
                .ok_or_else(|| {
                    RuntimeError::Internal(format!(
                        "entity claim {} lost but the winning row is missing",
                        spec.id
                    ))
                })?
        };
        if entity.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "entity claim {} is soft-deleted; restore it explicitly",
                entity.id
            )));
        }
        if entity.namespace != token.namespace().as_str()
            || entity.kind != spec.kind
            || entity.entity_type.as_deref() != entity_type.as_deref()
            || !entity.name.eq_ignore_ascii_case(&spec.name)
            || !entity
                .tags
                .iter()
                .any(|tag| tag.eq_ignore_ascii_case(&spec.identity_tag))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "entity claim {} belongs to a different record",
                entity.id
            )));
        }

        if !indexes_committed {
            self.ensure_claimed_entity_create_event(token, &entity)
                .await?;
            self.reindex_claimed_entity(token, &entity).await?;
        }
        Ok((entity, inserted))
    }

    /// A claimed row may survive a failed event append. Verify the event before
    /// a retry can report the row as registered.
    pub async fn ensure_claimed_entity_create_event(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<()> {
        if entity.namespace != token.namespace().as_str() || entity.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "entity {} is not a live row in the write namespace",
                entity.id
            )));
        }
        let events = self.events(token).map_err(|error| {
            RuntimeError::Internal(format!(
                "entity {} persists but its create event store is unavailable: {error}",
                entity.id
            ))
        })?;
        let filter = EventFilter {
            target_id: Some(entity.id),
            kinds: vec![EventKind::EntityCreated],
            verbs: vec!["create".into()],
            substrates: vec![SubstrateKind::Entity],
            after: Some(entity.created_at.saturating_sub(1)),
            ..EventFilter::default()
        };
        let page = PageRequest {
            offset: 0,
            limit: 1,
        };
        if !events
            .query_events(filter.clone(), page.clone())
            .await?
            .items
            .is_empty()
        {
            return Ok(());
        }

        let event = claimed_entity_create_event(entity);
        if let Err(error) = events.append_event(event).await {
            if events.query_events(filter, page).await?.items.is_empty() {
                return Err(RuntimeError::Internal(format!(
                    "entity {} persists but its create event failed: {error}",
                    entity.id
                )));
            }
        }
        Ok(())
    }

    /// Repair a claimed row after an earlier post-insert indexing failure.
    /// This path is strict: any failed index stage names the still-live id.
    pub async fn reindex_claimed_entity(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> RuntimeResult<()> {
        if entity.namespace != token.namespace().as_str() || entity.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "entity {} is not a live row in the write namespace",
                entity.id
            )));
        }
        let doc = entity_fts_document(entity);
        let embed_body = doc.body.clone();
        #[cfg(any(test, feature = "fault-injection"))]
        let fts_inject = consume_fault(&FTS_FAIL_NS, &entity.namespace);
        #[cfg(not(any(test, feature = "fault-injection")))]
        let fts_inject = false;
        let fts_result = if fts_inject {
            Err(RuntimeError::Internal("injected FTS failure".into()))
        } else {
            match self.text(token) {
                Ok(text) => text.upsert_document(doc).await.map_err(Into::into),
                Err(error) => Err(error),
            }
        };
        fts_result.map_err(|error| {
            RuntimeError::Internal(format!(
                "entity {} persists but its text index failed: {error}",
                entity.id
            ))
        })?;

        for model_name in self.registered_embedding_model_names() {
            let outcome = self
                .embed_document_with_model_outcome_for_token(token, &model_name, &embed_body)
                .await
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "entity {} persists but model {model_name} embedding failed: {error}",
                        entity.id
                    ))
                })?;
            #[cfg(any(test, feature = "fault-injection"))]
            let vector_inject = consume_fault(&VECTOR_FAIL_NS, &entity.namespace);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let vector_inject = false;
            if vector_inject {
                return Err(RuntimeError::Internal(format!(
                    "entity {} persists but model {model_name} vector indexing failed: injected vector failure",
                    entity.id
                )));
            }
            self.vectors_for_model(token, &model_name)
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "entity {} persists but model {model_name} vector store is unavailable: {error}",
                        entity.id
                    ))
                })?
                .insert(
                    entity.id,
                    SubstrateKind::Entity,
                    &entity.namespace,
                    "entity.body",
                    vec![outcome.vector],
                )
                .await
                .map_err(|error| {
                    RuntimeError::Internal(format!(
                        "entity {} persists but model {model_name} vector indexing failed: {error}",
                        entity.id
                    ))
                })?;
        }
        Ok(())
    }

    /// Create and persist a new entity.
    ///
    /// Indexing failures trigger compensation across the entity row, FTS
    /// document, and any vector models touched by this call. If compensation
    /// also fails, the returned structured internal error identifies possible
    /// partial persistence, includes both failure classes, and carries the
    /// entity ID as a reconciliation handle.
    // REASON: entity creation requires kind, type, name, description, properties, tags, and
    // namespace token — refactoring into a builder would add indirection without reducing
    // caller complexity; this signature mirrors the MCP verb surface directly.
    #[allow(clippy::too_many_arguments)]
    #[cfg(test)]
    pub(crate) async fn create_entity(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<Entity> {
        let (entity, _, degradations) = self
            .create_entity_with_embedding_report_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                Vec::new(),
            )
            .await?;
        legacy_post_commit_result("create_entity", entity.id, entity, degradations)
    }

    /// Create an entity with role-keyed bytes already published to `BlobStore`.
    ///
    /// Every [`NewAttachment`] carries a typed content reference, so malformed
    /// references cannot enter through this consumer seam. Blob existence is
    /// checked before the database write. The entity row and all attachment rows
    /// then commit in one storage transaction; the FTS/vector compensation path
    /// hard-deletes the entity and its attachments together if a later indexing
    /// step fails. Published bytes remain recoverable by the BlobStore grace-period
    /// orphan policy when any post-publication step fails.
    /// A bounded embedding returns a non-retryable error carrying the committed
    /// entity ID and truncation report; use the report-aware variant to receive
    /// the entity and report together.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_attachments(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<Entity> {
        let (entity, embedding, degradations) = self
            .create_entity_with_attachments_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                attachments,
            )
            .await?;
        legacy_post_commit_result_with_embedding(
            "create_entity_with_attachments",
            entity.id,
            entity,
            embedding,
            degradations,
        )
    }

    /// Create an entity with attachments and retain embedding truncation accounting.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_attachments_and_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        let (entity, embedding, degradations) = self
            .create_entity_with_attachments_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                attachments,
            )
            .await?;
        legacy_post_commit_result(
            "create_entity_with_attachments_and_report",
            entity.id,
            (entity, embedding),
            degradations,
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_entity_with_attachments_inner(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<(
        Entity,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        // Attachment rows are the process-wide BlobStore's liveness authority.
        // Validate placement before existence probes or any record write: pack
        // runtimes bound to a secondary backend must explicitly call `core()`.
        drop(self.attachments()?);
        let blob_store = self.blob_store().ok_or_else(|| {
            RuntimeError::Unconfigured(
                "create_entity_with_attachments requires an installed BlobStore".to_string(),
            )
        })?;
        let mut roles = std::collections::HashSet::with_capacity(attachments.len());
        for attachment in &attachments {
            attachment.validate()?;
            if !roles.insert(attachment.role.as_str()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "duplicate attachment role {:?}",
                    attachment.role
                )));
            }
        }
        for attachment in &attachments {
            if !blob_store.exists(&attachment.content_ref).await? {
                return Err(RuntimeError::InvalidInput(format!(
                    "create_entity_with_attachments requires a published blob; no object exists for {}",
                    attachment.content_ref
                )));
            }
        }
        let validated_type = self.validate_entity_type_for_kind(kind, entity_type)?;
        let (entity, embedding, degradations) = self
            .create_entity_with_embedding_report_inner(
                token,
                kind,
                validated_type.as_deref(),
                name,
                description,
                properties,
                tags,
                attachments,
            )
            .await?;
        Ok((entity, embedding, degradations))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_embedding_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<(Entity, crate::retrieval::EmbeddingTruncationReport)> {
        let (entity, embedding, degradations) = self
            .create_entity_with_embedding_report_inner(
                token,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                Vec::new(),
            )
            .await?;
        legacy_post_commit_result(
            "create_entity_with_embedding_report",
            entity.id,
            (entity, embedding),
            degradations,
        )
    }

    /// The committed entity and its non-retryable post-commit diagnostics.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_entity_with_post_commit_report(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
    ) -> RuntimeResult<(
        Entity,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        self.create_entity_with_embedding_report_inner(
            token,
            kind,
            entity_type,
            name,
            description,
            properties,
            tags,
            Vec::new(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_entity_with_embedding_report_inner(
        &self,
        token: &NamespaceToken,
        kind: &str,
        entity_type: Option<&str>,
        name: &str,
        description: Option<&str>,
        properties: Option<serde_json::Value>,
        tags: Vec<String>,
        attachments: Vec<NewAttachment>,
    ) -> RuntimeResult<(
        Entity,
        crate::retrieval::EmbeddingTruncationReport,
        Vec<PostCommitDegradation>,
    )> {
        self.validate_entity_kind(kind)?;
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;
        let ns = token.namespace().as_str();
        let mut entity = Entity::new(ns, kind, name).with_entity_type(entity_type);
        if let Some(d) = description {
            entity = entity.with_description(d);
        }
        if let Some(p) = properties {
            entity = entity.with_properties(p);
        }
        if !tags.is_empty() {
            entity = entity.with_tags(tags);
        }
        let projected_content_ref = attachments
            .iter()
            .find(|attachment| attachment.role == "content")
            .map(|attachment| attachment.content_ref.to_string());
        let attachment_rows: Vec<Attachment> = attachments
            .into_iter()
            .map(|attachment| {
                Attachment::from_new(
                    entity.id,
                    AttachmentSubstrate::Entity,
                    attachment,
                    entity.created_at,
                )
            })
            .collect();
        entity.content_ref = projected_content_ref.clone();
        entity = match prepare_entity_admission(token, EntityEntryPoint::Create, entity)? {
            EntityAdmission::Legacy(candidate) => candidate,
            EntityAdmission::Exempt(prepared) => {
                let entity = prepared.entity().clone();
                let (required, report) = self
                    .prepare_admitted_entity_indexes(token, &entity, &attachment_rows, true)
                    .await?;
                let plan = prepared.into_plan(
                    EntityCandidateMutation::CreateIfAbsent,
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
                        let degradations = self.record_created_entity_event(token, &entity).await;
                        return Ok((entity, report, degradations));
                    }
                    Ok(AtomicRunOutcome::RolledBack { failure, .. }) => {
                        return Err(RuntimeError::Internal(format!(
                            "entity finalization rolled back: {failure:?}"
                        )));
                    }
                    Err(error) => return Err(RuntimeError::Storage(error.0)),
                }
            }
        };
        self.entities(token)?
            .upsert_entity_with_attachments(entity.clone(), attachment_rows)
            .await?;
        entity.content_ref = projected_content_ref;

        let doc = entity_fts_document(&entity);
        let embed_body = doc.body.clone();

        // FTS step — compensate entity row on failure (mirrors create_note_inner).
        {
            #[cfg(any(test, feature = "fault-injection"))]
            let fts_inject = consume_fault(&FTS_FAIL_NS, ns);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let fts_inject = false;
            let fts_result: RuntimeResult<()> = if fts_inject {
                Err(RuntimeError::Internal("injected FTS failure".to_string()))
            } else {
                match self.text(token) {
                    Ok(fts) => fts.upsert_document(doc).await.map_err(RuntimeError::from),
                    Err(e) => Err(e),
                }
            };
            if let Err(e) = fts_result {
                let cleanup_errors = self
                    .compensate_entity_create(token, entity.id, ns, &[])
                    .await;
                return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
            }
        }

        // Vector embedding + insert step — compensate entity row + FTS doc on failure.
        // Fan out to ALL registered models (mirrors create_note_inner multi-model path).
        let embed_model_names = {
            let names = self.registered_embedding_model_names();
            if names.is_empty() {
                vec![]
            } else {
                names
            }
        };

        let mut embedding_report = crate::retrieval::EmbeddingTruncationReport::default();
        if embed_model_names.len() == 1 {
            let model_name = &embed_model_names[0];
            let vec_result = self
                .embed_document_with_model_outcome_for_token(token, model_name, &embed_body)
                .await;

            #[cfg(any(test, feature = "fault-injection"))]
            let vec_inject = consume_fault(&VECTOR_FAIL_NS, ns);
            #[cfg(not(any(test, feature = "fault-injection")))]
            let vec_inject = false;
            let vec_result: RuntimeResult<crate::retrieval::DocumentEmbeddingOutcome> =
                if vec_inject {
                    Err(RuntimeError::Internal(
                        "injected vector failure".to_string(),
                    ))
                } else {
                    vec_result
                };

            let single_result: RuntimeResult<()> = match vec_result {
                Ok(outcome) => {
                    embedding_report.observe(&outcome);
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => vs
                            .insert(
                                entity.id,
                                SubstrateKind::Entity,
                                ns,
                                "entity.body",
                                vec![outcome.vector],
                            )
                            .await
                            .map_err(RuntimeError::from),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(e) = single_result {
                let cleanup_errors = self
                    .compensate_entity_create(
                        token,
                        entity.id,
                        ns,
                        std::slice::from_ref(model_name),
                    )
                    .await;
                return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
            }
        } else if !embed_model_names.is_empty() {
            // Multi-model path: embed with each model in parallel, then insert sequentially
            // with inserted_models tracking for rollback on partial failure.
            let rt_clone = self.clone();
            let body_owned = embed_body.clone();
            let usage_ctx = crate::usage::current();
            let mut join_set = tokio::task::JoinSet::new();
            for (idx, model_name) in embed_model_names.iter().enumerate() {
                let rt = rt_clone.clone();
                let text = body_owned.clone();
                let name = model_name.clone();
                let ctx = usage_ctx.clone();
                let token = (*token).clone();
                join_set.spawn(crate::runtime::inherit_request_embedder_scope(async move {
                    let fut = rt.embed_document_with_model_outcome_for_token(&token, &name, &text);
                    let result = match ctx {
                        Some(ctx) => crate::usage::scope(ctx, fut).await,
                        None => fut.await,
                    };
                    (idx, result)
                }));
            }
            // The first failed or panicked handle aborts and detaches its
            // siblings. Embed usage is counted at dispatch, so a synchronous
            // provider winding down in the background cannot change it.
            let outcomes = match drain_embed_join_set(join_set, embed_model_names.len()).await {
                Ok(outcomes) => outcomes,
                Err(e) => {
                    let cleanup_errors = self
                        .compensate_entity_create(token, entity.id, ns, &[])
                        .await;
                    return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
                }
            };
            // TODO(P2): parallelize vector inserts
            let mut inserted_models: Vec<String> = Vec::with_capacity(embed_model_names.len());
            for (model_name, outcome) in embed_model_names.iter().zip(outcomes) {
                embedding_report.observe(&outcome);
                // Count-targetable fault injection for multi-model insert path.
                #[cfg(any(test, feature = "fault-injection"))]
                let count_inject = VECTOR_FAIL_AFTER.with(|cell| match cell.get() {
                    Some(0) => {
                        cell.set(None);
                        true
                    }
                    Some(n) => {
                        cell.set(Some(n - 1));
                        false
                    }
                    None => false,
                });
                #[cfg(not(any(test, feature = "fault-injection")))]
                let count_inject = false;

                let insert_result = if count_inject {
                    Err(RuntimeError::Internal(
                        "injected vector insert failure".to_string(),
                    ))
                } else {
                    match self.vectors_for_model(token, model_name) {
                        Ok(vs) => vs
                            .insert(
                                entity.id,
                                SubstrateKind::Entity,
                                ns,
                                "entity.body",
                                vec![outcome.vector],
                            )
                            .await
                            .map_err(RuntimeError::from),
                        Err(e) => Err(e),
                    }
                };
                if let Err(e) = insert_result {
                    // Include the model whose INSERT returned an error: a backend
                    // error does not prove the write had no side effects.
                    let mut cleanup_models = inserted_models.clone();
                    cleanup_models.push(model_name.clone());
                    let cleanup_errors = self
                        .compensate_entity_create(token, entity.id, ns, &cleanup_models)
                        .await;
                    return Err(Self::entity_create_failure(entity.id, e, cleanup_errors));
                }
                inserted_models.push(model_name.clone());
            }
        }

        let degradations = self.record_created_entity_event(token, &entity).await;
        Ok((entity, embedding_report, degradations))
    }

    async fn record_created_entity_event(
        &self,
        token: &NamespaceToken,
        entity: &Entity,
    ) -> Vec<PostCommitDegradation> {
        // The arrival event, appended only after every compensating step has had
        // its chance to fire: a create that rolled back returns above and never
        // reaches here, so the event plane cannot name an entity that does not
        // exist. Deletes and updates already emitted theirs; creates did not,
        // which left the audit trail able to say what left the graph and not
        // what entered it.
        let created_event = khive_storage::event::Event::new(
            entity.namespace.clone(),
            "create",
            EventKind::EntityCreated,
            SubstrateKind::Entity,
            "",
        )
        .with_target(entity.id)
        .with_payload(serde_json::json!({
            "id": entity.id,
            "namespace": entity.namespace,
            "kind": entity.kind,
        }));
        let event_result = match self.events(token) {
            Ok(store) => store
                .append_event(created_event)
                .await
                .map_err(RuntimeError::from),
            Err(error) => Err(error),
        };
        let mut degradations = Vec::new();
        if let Err(error) = event_result {
            record_post_commit_degradation(
                &mut degradations,
                "create_entity",
                entity.id,
                "event_append",
                error,
            );
        }

        degradations
    }
}
