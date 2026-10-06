use super::{
    kind_owned_properties, merge_properties, note_update_values_equal,
    owner_established_property_named_in, reject_pack_managed_schedule_mutation,
    stale_note_snapshot_error, EntityDedupMergePolicy, KhiveRuntime, NamespaceToken, NotePatch,
    RuntimeError, RuntimeResult, Uuid, Value,
};

impl KhiveRuntime {
    /// Apply a note patch to exactly the supplied read snapshot without
    /// fetching the row again. The caller must persist it through
    /// [`Self::update_note_from_snapshot_with_embedding_report`] or a write
    /// plan guarded by the snapshot's `updated_at`/`deleted_at` values.
    pub(crate) async fn prepare_update_note_from_snapshot(
        &self,
        _token: &NamespaceToken,
        mut note: khive_storage::note::Note,
        patch: NotePatch,
    ) -> RuntimeResult<(khive_storage::note::Note, bool, bool)> {
        if note.properties.as_ref().is_some_and(|properties| {
            properties
                .as_object()
                .is_some_and(|map| map.contains_key(crate::secret_gate::RESERVED_WEB_RECEIPT_KEY))
        }) {
            return Err(RuntimeError::InvalidInput(
                "web receipt notes are immutable through generic update".into(),
            ));
        }
        // The stored row as read. A no-op answers with this, not with the
        // patched snapshot: the patch may differ from the row in ways the
        // no-op decision ignores (tag order), and nothing was written.
        let stored = note.clone();
        let original_name = note.name.clone();
        let original_content = note.content.clone();
        let original_salience = note.salience;
        let original_decay_factor = note.decay_factor;
        let original_properties = note.properties.clone();
        let original_status = note.status.clone();
        if patch
            .update_policy
            .kind
            .as_deref()
            .is_some_and(|kind| kind != note.kind)
        {
            return Err(RuntimeError::InvalidInput(
                "note update policy does not match the stored note kind".into(),
            ));
        }
        if patch.content.is_some() || patch.properties.is_some() {
            if let Some(error) = self.stream_member_error(&note).await? {
                return Err(error);
            }
        }
        crate::secret_gate::reject_reserved_secret_gate_property(patch.properties.as_ref())?;
        if let Some(ref content) = patch.content {
            crate::secret_gate::check_at(content, "note", "content")?;
        }
        if let Some(Some(ref name)) = patch.name {
            crate::secret_gate::check_at(name, "note", "name")?;
        }
        if let Some(ref props) = patch.properties {
            crate::secret_gate::check_json_at(props, "note", "properties")?;
        }

        reject_pack_managed_schedule_mutation(&note, "update")?;

        let mut text_changed = false;

        if let Some(name_patch) = patch.name {
            text_changed |= note.name != name_patch;
            note.name = name_patch;
        }
        if let Some(content) = patch.content {
            text_changed |= note.content != content;
            note.content = content;
        }
        if let Some(salience_patch) = patch.salience {
            // Reject invalid salience rather than silently clamping caller input.
            if let Some(s) = salience_patch {
                if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "salience must be a finite value in [0.0, 1.0]; got {s}"
                    )));
                }
            }
            note.salience = salience_patch;
        }
        if let Some(decay_patch) = patch.decay_factor {
            // Reject invalid decay_factor rather than silently clamping caller input.
            if let Some(d) = decay_patch {
                if !d.is_finite() || d < 0.0 {
                    return Err(crate::RuntimeError::InvalidInput(format!(
                        "decay_factor must be a finite value >= 0.0; got {d}"
                    )));
                }
            }
            note.decay_factor = decay_patch;
        }
        if let Some(props) = patch.properties {
            // Kind-owned identity is protected below pack hooks, including
            // direct runtime and atomic/proposal update preparation. The merge
            // path restores these same keys on its surviving row.
            let owned_keys = kind_owned_properties(&note.kind);
            if !owned_keys.is_empty() {
                let object = props.as_object().ok_or_else(|| {
                    if note.kind == "message" {
                        RuntimeError::InvalidInput(
                            "properties on a `message` note must be patched with an object: a \
                             non-object patch would replace the transport-owned quarantine and \
                             channel provenance established by `comm.ingest`"
                                .into(),
                        )
                    } else {
                        RuntimeError::InvalidInput(format!(
                            "properties on a `{}` note must be patched with an object; \
                             a non-object patch would erase its owner-established identity",
                            note.kind
                        ))
                    }
                })?;
                if let Some(named) = owned_keys.iter().find(|key| object.contains_key(**key)) {
                    if note.kind == "message" {
                        return Err(RuntimeError::InvalidInput(format!(
                            "`{named}` is transport-owned on a `message` note and cannot be patched; \
                             only `comm.ingest` may establish quarantine disposition and channel \
                             provenance"
                        )));
                    }
                    return Err(RuntimeError::InvalidInput(format!(
                        "`{named}` is not patchable on a `{}` note; \
                         use `comm.heartbeat` to report health without changing the row's identity",
                        note.kind
                    )));
                }
            }
            // On a pack-owned note kind, the properties in
            // `OWNER_ESTABLISHED_PROPERTIES` are established by the owning pack
            // and read back by it to decide something structural — who wrote
            // the record and when, which author-side record it copies, which
            // conversation it belongs to. A caller cannot patch them here.
            // Only a patch that *names* one of them is refused, and naming is
            // the exact test: the merge below is `PreferFrom`, so a patch that
            // names an owned key would overwrite it while a patch that does
            // not name it leaves it intact. Every other key still merges
            // normally — arbitrary metadata on a pack-owned record (a
            // `blocked_on` note on a `task`) has no other write path and must
            // keep working.
            if self.is_pack_owned_note_kind(&note.kind) {
                // A non-object patch names nothing, so it slips past the
                // named-key check below and then takes `merge_json`'s
                // non-object `PreferFrom` arm, which replaces the whole
                // property object rather than merging into it — erasing
                // every owned key. Refused on every pack-owned kind, not only
                // rows that currently carry an owned key, so an identical
                // call cannot succeed or fail on state the caller cannot see.
                if !props.is_object() {
                    return Err(RuntimeError::InvalidInput(format!(
                        "properties on a `{}` note must be patched with an object: a non-object \
                         patch names no key, so it would replace the whole property object rather \
                         than merging into it. Pass an object containing the keys you intend to \
                         set.",
                        note.kind
                    )));
                }
                if let Some(named) = owner_established_property_named_in(&props) {
                    return Err(RuntimeError::InvalidInput(format!(
                        "`{named}` is not patchable on a `{}` note: the pack that owns this \
                         kind establishes it and reads it back — to decide how the record is \
                         attributed and grouped, or to reproduce it verbatim when the record \
                         is re-emitted — so it is written by the owner and immutable to a \
                         caller patch. Patch any other property key here, or omit \
                         `{named}` from this patch.",
                        note.kind
                    )));
                }
            }
            let incoming_properties = Some(props);
            let (mut merged, _) = merge_properties(
                &note.properties,
                &incoming_properties,
                EntityDedupMergePolicy::PreferFrom,
            );
            if let Some(properties) = merged.as_mut().and_then(Value::as_object_mut) {
                for key in patch.update_policy.null_clearing_properties {
                    if incoming_properties
                        .as_ref()
                        .and_then(|incoming| incoming.get(*key))
                        .is_some_and(Value::is_null)
                    {
                        properties.remove(*key);
                    }
                }
            }
            note.properties = merged;
        }
        if let Some(status) = patch.kind_status {
            note.status = status;
        }

        // The whole-note CAS persists the merged properties, including keys
        // carried from the snapshot when the patch changes another field.
        crate::secret_gate::reject_reserved_secret_gate_property(note.properties.as_ref())?;

        // JSON object key order is not meaningful to callers. Tags are also
        // set-like in every existing note reader, so their order is ignored
        // for the no-op decision while duplicate entries remain meaningful.
        // All other arrays retain ordinary JSON ordering semantics.
        let changed = original_name != note.name
            || original_content != note.content
            || original_salience != note.salience
            || original_decay_factor != note.decay_factor
            || !note_update_values_equal(&original_properties, &note.properties)
            || original_status != note.status;
        if !changed {
            return Ok((stored, text_changed, false));
        }

        // `updated_at` is also the optimistic-concurrency revision for
        // full-note replacement. Make it strictly advance even when two
        // operations land inside one clock microsecond. Saturation is not a
        // valid fallback: reusing i64::MAX would make the CAS accept a write
        // without advancing its revision.
        let minimum_updated_at = note.updated_at.checked_add(1).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "note {} updated_at is already at i64::MAX and cannot advance",
                note.id
            ))
        })?;
        note.updated_at = chrono::Utc::now()
            .timestamp_micros()
            .max(minimum_updated_at);
        Ok((note, text_changed, true))
    }

    /// Patch-style note update.
    #[cfg(test)]
    pub(crate) async fn update_note(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: NotePatch,
    ) -> RuntimeResult<khive_storage::note::Note> {
        Ok(self
            .update_note_with_embedding_report(token, id, patch)
            .await?
            .0)
    }

    pub async fn update_note_with_embedding_report(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        patch: NotePatch,
    ) -> RuntimeResult<(
        khive_storage::note::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let snapshot = self
            .notes(token)?
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        self.update_note_from_snapshot_with_embedding_report(token, snapshot, patch)
            .await
    }

    /// Patch and persist one note from a caller-owned read snapshot.
    ///
    /// This is the canonical seam for kind hooks that normalize coupled
    /// fields from the current note. The same snapshot feeds normalization,
    /// patch application, and the compare-and-swap write; a concurrent note
    /// change therefore refuses the write instead of persisting derivations
    /// computed from stale state.
    pub async fn update_note_from_snapshot_with_embedding_report(
        &self,
        token: &NamespaceToken,
        snapshot: khive_storage::note::Note,
        patch: NotePatch,
    ) -> RuntimeResult<(
        khive_storage::note::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let (note, plan) = self
            .prepare_versioned_note_update(token, snapshot, patch)
            .await?;
        self.commit_prepared_note_update(token, note, crate::AtomicOpPlan::Update(Box::new(plan)))
            .await
    }

    /// Commit a normalized and validated kind-owned update, including its typed
    /// graph companions. Callers must first run `prepare_note_update_policy`
    /// against this exact snapshot and pass the policy it returned; the shared
    /// atomic prepare seam checks all patch fields before asking the kind hook
    /// to derive any graph effects.
    pub async fn update_note_from_snapshot_with_kind_effects(
        &self,
        token: &NamespaceToken,
        snapshot: khive_storage::Note,
        args: &Value,
        policy: crate::NoteUpdatePolicy,
        registry: &crate::VerbRegistry,
    ) -> RuntimeResult<(
        khive_storage::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let (note, plan) = crate::atomic_prepare::prepare_update_from_note_snapshot(
            self, token, args, None, snapshot, policy, registry,
        )
        .await?;
        self.commit_prepared_note_update(token, note, plan).await
    }

    async fn commit_prepared_note_update(
        &self,
        token: &NamespaceToken,
        note: khive_storage::Note,
        plan: crate::AtomicOpPlan,
    ) -> RuntimeResult<(
        khive_storage::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        let id = note.id;
        use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicRunOutcome};
        match run_atomic_unit(self.sql().as_ref(), vec![plan]).await {
            Ok(AtomicRunOutcome::Committed { post_commit }) => {
                let outcomes = crate::atomic_prepare::apply_post_commit_effects_with_report(
                    self,
                    token,
                    post_commit,
                )
                .await?;
                let report = outcomes
                    .into_iter()
                    .next()
                    .map(|outcome| outcome.truncation)
                    .unwrap_or_default();
                Ok((note, report))
            }
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::NoteConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::GuardFailed { .. },
                ..
            }) => Err(stale_note_snapshot_error(id)),
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("note update rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }
}
