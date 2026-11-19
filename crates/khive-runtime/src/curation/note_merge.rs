//! Asynchronous note merge, with or without a caller-supplied guard.

use super::*;

impl KhiveRuntime {
    /// Merge `from_id` note into `into_id` note.
    ///
    /// Both notes must exist in the namespace and have the same `kind`. Content is merged
    /// per `content_strategy`. Properties are merged per `strategy`. `from_id` is
    /// tombstoned (status='deleted', deleted_at set). Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating
    /// any rows, edges, or indexes.
    /// The NoteMerged event, including destructive edge preimages, commits in
    /// the same SQL transaction as the note and edge changes.
    pub async fn merge_note(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_note_with_reason(
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

    /// Merge `from_id` note into `into_id` note and include an optional audit reason.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_note_with_reason(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_note_with_guard(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            reason,
            None,
        )
        .await
        .map(|(summary, _)| summary)
    }

    /// Merge `from_id` note into `into_id` note, refusing unless the caller's
    /// guard still holds.
    ///
    /// The guard names the version of each note the caller read, facts it relied
    /// on (see [`MergeAssertion`]), property values that replace the survivor's,
    /// and one annotation for the provenance entry. All of it is checked inside
    /// the merge transaction, on the writer connection, after both notes are read
    /// there and before the first write; a refusal writes nothing. The returned
    /// `kept_version` is the survivor's committed version, so the caller can
    /// merge the next duplicate into the same survivor without reading it again.
    ///
    /// With no guard this is [`Self::merge_note_with_reason`]: the same
    /// statements in the same order, with the same errors and effects.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields plus the guard; a builder would only move
    // that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_note_guarded(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
        guard: NoteMergeGuard,
    ) -> RuntimeResult<GuardedNoteMerge> {
        let (summary, kept_version) = self
            .merge_note_with_guard(
                token,
                into_id,
                from_id,
                strategy,
                content_strategy,
                dry_run,
                reason,
                Some(guard),
            )
            .await?;
        Ok(GuardedNoteMerge {
            summary,
            kept_version,
        })
    }

    /// Merge `from_id` note into `into_id` note and include an optional audit reason.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn merge_note_with_guard(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
        merge_guard: Option<NoteMergeGuard>,
    ) -> RuntimeResult<(MergeSummary, i64)> {
        if let Some(reason) = reason.as_deref() {
            crate::secret_gate::check_at(reason, "merge", "reason")?;
        }
        if let Some(merge_guard) = merge_guard.as_ref() {
            merge_guard.check_values()?;
        }
        if into_id == from_id {
            return Err(RuntimeError::InvalidInput(
                "cannot merge a note into itself".into(),
            ));
        }
        let ns = token.namespace().as_str().to_string();
        let fts_table = "fts_notes".to_string();
        // Keep deletion, table preparation, and survivor reindex on the same
        // immutable registry view; see the entity merge path above.
        let embedding_plan = EmbeddingModelPlan::capture(self);
        let vec_tables = embedding_plan.vector_tables();
        let pack_rules = self.pack_edge_rules();

        let note_store = self.notes(token)?;
        let into_note = note_store
            .get_note(into_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("not found in this namespace".into()))?;
        Self::ensure_namespace(&into_note.namespace, &ns)?;

        let from_note = note_store
            .get_note(from_id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound("not found in this namespace".into()))?;
        Self::ensure_namespace(&from_note.namespace, &ns)?;

        if !dry_run {
            for note in [&into_note, &from_note] {
                if let Some(error) = self.stream_member_error(note).await? {
                    return Err(error);
                }
            }
        }
        reject_pack_managed_schedule_mutation(&into_note, "merge")?;
        reject_pack_managed_schedule_mutation(&from_note, "merge")?;

        let _ = self.graph(token)?;
        let _ = self.text_for_notes(token)?;
        let _ = self.events(token)?;
        for model_name in embedding_plan.model_names() {
            let _ = self.vectors_for_model(token, model_name)?;
        }

        // Resolved here, where the runtime's installed pack-kind list is in
        // reach; `merge_note_sql` runs on the writer connection with no runtime
        // handle. Both notes share a kind (checked inside), so the into-note's
        // kind decides for the merge.
        let preserve_owner_established = self.is_pack_owned_note_kind(&into_note.kind);

        let pool = self.backend().pool_arc();
        let writer_task = pool
            .writer_task_for_runtime_write(RuntimeWriteOperation::MergeNote)
            .map_err(RuntimeError::Storage)?;
        let event_context = MergeEventContext {
            attribution: EventAttribution::from_token(token),
            reason,
            force: false,
            strategy,
            content_strategy,
            kind: EventKind::NoteMerged,
            substrate: SubstrateKind::Note,
            event_id: None,
        };

        let (mut summary, updated_note) = if let Some(writer_task) = writer_task {
            writer_task
                .send(move |conn| {
                    merge_note_sql(
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
                        preserve_owner_established,
                        MergeTxLimits::default(),
                        Some(event_context),
                        merge_guard,
                    )
                    .map_err(|e| {
                        e.into_storage_error(khive_storage::StorageCapability::Notes, "merge_note")
                    })
                })
                .await
                .inspect_err(|error| khive_storage::usage::account_event_write(Err(error)))
                .map_err(map_merge_note_storage_error)?
        } else {
            tokio::task::spawn_blocking(move || {
                let guard = pool.writer()?;
                let mut refusal = None;
                let result = guard.transaction(|conn| {
                    merge_note_sql(
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
                        preserve_owner_established,
                        MergeTxLimits::default(),
                        Some(event_context),
                        merge_guard,
                    )
                    .map_err(|error| match error {
                        MergeSqlError::Sqlite(error) => error,
                        MergeSqlError::Refusal(error) => {
                            refusal = Some(error);
                            SqliteError::InvalidData(
                                "note merge refused by transactional policy".to_string(),
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
                "merge_note: transaction materialization budget"
            );
        }

        if !dry_run {
            if !embedding_plan.is_empty() {
                #[cfg(any(test, feature = "fault-injection"))]
                let reindex_result =
                    if crate::operations::consume_fts_fail_fault(&updated_note.namespace) {
                        Err(RuntimeError::Internal("injected FTS failure".to_string()))
                    } else {
                        self.reindex_note_report_with_plan(token, &updated_note, &embedding_plan)
                            .await
                    };
                #[cfg(not(any(test, feature = "fault-injection")))]
                let reindex_result = self
                    .reindex_note_report_with_plan(token, &updated_note, &embedding_plan)
                    .await;

                match reindex_result {
                    Ok(report) => {
                        summary.embedding_truncation = report.truncation;
                        if !report.failures.is_empty() {
                            summary.post_commit_reindex_error = Some(
                                report
                                    .failures
                                    .iter()
                                    .map(|failure| {
                                        format!(
                                            "model {} {}: {}",
                                            failure.model,
                                            failure.stage.as_str(),
                                            failure.error
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("; "),
                            );
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            into_id = %summary.kept_id,
                            from_id = %summary.removed_id,
                            error = %error,
                            "merge_note: committed merge but survivor reindex failed"
                        );
                        summary.post_commit_reindex_error = Some(error.to_string());
                    }
                }
            }
            // The note row is committed even when no embedding model is
            // registered or the post-commit reindex reports an error.
            self.fire_note_mutation_hook(&updated_note.kind, updated_note.id)
                .await;
        }

        Ok((summary, updated_note.version))
    }
}
