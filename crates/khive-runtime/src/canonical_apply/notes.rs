use khive_changeset::{UpdateOp, UpdatePatch};
use khive_storage::Note;
use serde_json::Value;

use crate::atomic_plan::UpdatePlan;
use crate::atomic_runner::{AtomicOpFailure, AtomicOpPlan};
use crate::curation::NotePatch;
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult, VerbRegistry};

#[derive(Clone, Debug)]
pub(super) struct PreparedCanonicalNote {
    token: NamespaceToken,
    snapshot: Note,
    patch: NotePatch,
    replace_properties: bool,
    replace_tags: bool,
    pack_owned: bool,
    metadata_only: bool,
    prepared_with_owner: bool,
    plan: UpdatePlan,
}

pub(super) async fn prepare(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    current: Note,
    op: &UpdateOp,
) -> RuntimeResult<PreparedCanonicalNote> {
    // Refuse malformed stored aliases before any owner hook sees the row.
    super::projection::note(&current)?;
    let UpdatePatch::Note(canonical) = op.patch() else {
        return Err(RuntimeError::InvalidInput(
            "expected canonical note update".into(),
        ));
    };
    let mut args = serde_json::json!({"id": current.id});
    if let Some(content) = &canonical.content {
        args["content"] = content.clone().into();
    }
    if let Some(salience) = canonical.salience {
        args["salience"] = serde_json::json!(salience);
    }
    if let Some(decay) = canonical.decay_factor {
        args["decay_factor"] = serde_json::json!(decay);
    }
    if let Some(properties) = &canonical.properties {
        if properties.contains_key("tags") {
            return Err(RuntimeError::InvalidInput(
                "canonical note properties must not contain the tags storage alias".into(),
            ));
        }
        args["properties"] = serde_json::to_value(properties)
            .map_err(|_| RuntimeError::InvalidInput("invalid canonical note properties".into()))?;
        restore_tags(&mut args["properties"], &current)?;
    }
    if let Some(tags) = &canonical.tags {
        args["tags"] = serde_json::json!(tags);
    }
    let prepared_with_owner = registry.find_kind_hook(&current.kind).is_some();
    let policy = registry
        .prepare_note_update_policy(runtime, token, &current, &mut args)
        .await?;
    let patch = NotePatch::new(
        None,
        args.get("content")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|_| RuntimeError::InvalidInput("invalid normalized note content".into()))?,
        float_patch(&args, "salience")?,
        float_patch(&args, "decay_factor")?,
        args.get("properties").cloned(),
    )
    .with_update_policy(policy);
    let member_error = runtime.stream_member_error(&current).await?;
    let metadata_only = member_error.is_some();
    if patch.content.is_some() || patch.properties.is_some() {
        if let Some(error) = member_error {
            return Err(error);
        }
    }
    let pack_owned = runtime.is_pack_owned_note_kind(&current.kind);
    let now = chrono::Utc::now().timestamp_micros();
    let (updated, text_changed, changed) = KhiveRuntime::apply_note_update_fields(
        current.clone(),
        patch.clone(),
        pack_owned,
        canonical.properties.is_some(),
        true,
        now,
    )?;
    let (_, mut plan) = KhiveRuntime::materialize_versioned_note_update(
        token,
        current.clone(),
        updated,
        text_changed,
        changed,
        patch.write_options.clone(),
        metadata_only,
        now,
    )?;
    crate::atomic_prepare::attach_note_update_effects(
        runtime, token, registry, &current, &patch, &mut plan,
    )
    .await?;
    Ok(PreparedCanonicalNote {
        token: token.clone(),
        snapshot: current,
        patch,
        replace_properties: canonical.properties.is_some(),
        replace_tags: canonical.tags.is_some(),
        pack_owned,
        metadata_only,
        prepared_with_owner,
        plan,
    })
}

impl PreparedCanonicalNote {
    pub(super) fn snapshot(&self) -> &Note {
        &self.snapshot
    }

    pub(super) fn prepared_with_owner(&self) -> bool {
        self.prepared_with_owner
    }

    pub(super) fn projected(&self, now: i64) -> RuntimeResult<Note> {
        self.rematerialize(&self.snapshot, now)
            .map(|(note, _)| note)
    }

    fn rematerialize(&self, live: &Note, now: i64) -> RuntimeResult<(Note, UpdatePlan)> {
        super::projection::note(live)?;
        let mut patch = self.patch.clone();
        if self.replace_properties && !self.replace_tags {
            if let Some(properties) = patch.properties.as_mut() {
                // A map-only replacement retains the alias read from this
                // applying writer, including its absence or explicit [].
                restore_tags(properties, live)?;
            }
        }
        let (updated, text_changed, changed) = KhiveRuntime::apply_note_update_fields(
            live.clone(),
            patch.clone(),
            self.pack_owned,
            self.replace_properties,
            true,
            now,
        )?;
        KhiveRuntime::materialize_versioned_note_update(
            &self.token,
            live.clone(),
            updated,
            text_changed,
            changed,
            patch.write_options,
            self.metadata_only,
            now,
        )
    }
}

pub(super) fn materialize(
    token: &NamespaceToken,
    live: Note,
    prepared: &PreparedCanonicalNote,
    now: i64,
) -> Result<AtomicOpPlan, AtomicOpFailure> {
    if prepared.prepared_with_owner {
        // The caller has checked the live owner lookup and full revision
        // guard. Preserve the complete owner-derived companion plan.
        return Ok(AtomicOpPlan::Update(Box::new(prepared.plan.clone())));
    }
    let mut patch = prepared.patch.clone();
    if prepared.replace_properties && !prepared.replace_tags {
        if let Some(properties) = patch.properties.as_mut() {
            restore_tags(properties, &live).map_err(failure)?;
        }
    }
    super::projection::note(&live).map_err(|error| failure(error.into()))?;
    let snapshot = live.clone();
    let (updated, text_changed, changed) = KhiveRuntime::apply_note_update_fields(
        live,
        patch.clone(),
        prepared.pack_owned,
        prepared.replace_properties,
        true,
        now,
    )
    .map_err(failure)?;
    let (_, plan) = KhiveRuntime::materialize_versioned_note_update(
        token,
        snapshot,
        updated,
        text_changed,
        changed,
        patch.write_options,
        prepared.metadata_only,
        now,
    )
    .map_err(failure)?;
    Ok(AtomicOpPlan::Update(Box::new(plan)))
}

fn restore_tags(properties: &mut Value, live: &Note) -> RuntimeResult<()> {
    let map = properties.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidInput("canonical note properties must be an object".into())
    })?;
    match live.properties.as_ref().and_then(|value| value.get("tags")) {
        Some(tags) => {
            serde_json::from_value::<Vec<String>>(tags.clone()).map_err(|_| {
                RuntimeError::InvalidInput("stored note tags must be an array of strings".into())
            })?;
            map.insert("tags".into(), tags.clone());
        }
        None => {
            map.remove("tags");
        }
    }
    Ok(())
}

fn float_patch(args: &Value, field: &str) -> RuntimeResult<Option<Option<f64>>> {
    args.get(field)
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|_| RuntimeError::InvalidInput(format!("invalid normalized note {field}")))
}

fn failure(error: RuntimeError) -> AtomicOpFailure {
    AtomicOpFailure::SqlError {
        statement_label: Some("canonical-note-update".into()),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_replacement_retains_writer_tags_and_removes_old_keys() {
        let live = Note::new("local", "observation", "same")
            .with_properties(json!({"old": 1, "tags": ["writer"]}));
        let mut properties = json!({"new": 2});
        restore_tags(&mut properties, &live).unwrap();
        let (updated, _, changed) = KhiveRuntime::apply_note_update_fields(
            live,
            NotePatch::new(None, None, None, None, Some(properties)),
            false,
            true,
            true,
            1,
        )
        .unwrap();
        assert!(changed);
        assert_eq!(
            updated.properties,
            Some(json!({"new": 2, "tags": ["writer"]}))
        );
    }

    #[test]
    fn tag_only_clear_preserves_other_writer_properties() {
        let live = Note::new("local", "observation", "same")
            .with_properties(json!({"concurrent": 7, "tags": ["old"]}));
        let (updated, _, _) = KhiveRuntime::apply_note_update_fields(
            live,
            NotePatch::new(None, None, None, None, Some(json!({"tags": []}))),
            false,
            false,
            true,
            1,
        )
        .unwrap();
        assert_eq!(
            updated.properties,
            Some(json!({"concurrent": 7, "tags": []}))
        );
    }

    #[test]
    fn replacement_cannot_erase_or_retarget_owner_identity() {
        let live = Note::new("local", "task", "same")
            .with_properties(json!({"from_actor": "owner", "old": 1}));
        for replacement in [json!({}), json!({"from_actor": "other"})] {
            assert!(KhiveRuntime::apply_note_update_fields(
                live.clone(),
                NotePatch::new(None, None, None, None, Some(replacement)),
                true,
                true,
                true,
                1,
            )
            .is_err());
        }
        let (updated, _, _) = KhiveRuntime::apply_note_update_fields(
            live,
            NotePatch::new(None, None, None, None, Some(json!({"from_actor": "owner"}))),
            true,
            true,
            true,
            1,
        )
        .unwrap();
        assert_eq!(updated.properties, Some(json!({"from_actor": "owner"})));
    }

    #[test]
    fn malformed_storage_tags_refuse_instead_of_coercing() {
        for tags in [json!("old"), json!([1]), Value::Null] {
            let live =
                Note::new("local", "observation", "same").with_properties(json!({"tags": tags}));
            assert!(restore_tags(&mut json!({}), &live).is_err());
        }
    }

    #[test]
    fn ordinary_merge_and_tag_order_noop_remain_distinct_from_canonical() {
        let live = Note::new("local", "observation", "same")
            .with_properties(json!({"keep": 1, "tags": ["a", "b"]}));
        let patch = NotePatch::new(None, None, None, None, Some(json!({"tags": ["b", "a"]})));
        let (ordinary, _, changed) = KhiveRuntime::apply_note_update_fields(
            live.clone(),
            patch.clone(),
            false,
            false,
            false,
            1,
        )
        .unwrap();
        assert!(!changed);
        assert_eq!(ordinary, live);
        let (canonical, _, changed) =
            KhiveRuntime::apply_note_update_fields(live, patch, false, false, true, 1).unwrap();
        assert!(changed);
        assert_eq!(
            canonical.properties,
            Some(json!({"keep": 1, "tags": ["b", "a"]}))
        );
    }
}
