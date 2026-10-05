use super::*;

use async_trait::async_trait;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
use serde_json::json;

use khive_types::Namespace;

use crate::embedder_registry::EmbedderProvider;
use crate::runtime::RuntimeConfig;

/// Owns a file-backed runtime and removes its database directory after shutdown.
struct TestRuntime {
    runtime: KhiveRuntime,
    _temp_dir: tempfile::TempDir,
}

impl std::ops::Deref for TestRuntime {
    type Target = KhiveRuntime;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

const STUB_MODEL: &str = "stub-adr099-b3";
const SECOND_STUB_MODEL: &str = "stub-adr044-a4-second";
const STUB_DIMS: usize = 4;

struct StubService;

#[async_trait]
impl EmbeddingService for StubService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.5_f32; STUB_DIMS]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        STUB_MODEL
    }
}

struct StubProvider;

struct SecondStubProvider;

#[async_trait]
impl EmbedderProvider for SecondStubProvider {
    fn name(&self) -> &str {
        SECOND_STUB_MODEL
    }

    fn dimensions(&self) -> usize {
        STUB_DIMS
    }

    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(StubService))
    }
}

#[async_trait]
impl EmbedderProvider for StubProvider {
    fn name(&self) -> &str {
        STUB_MODEL
    }

    fn dimensions(&self) -> usize {
        STUB_DIMS
    }

    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(StubService))
    }
}

fn scratch_runtime() -> TestRuntime {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("atomic_prepare_reindex.db");
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    TestRuntime {
        runtime,
        _temp_dir: dir,
    }
}

/// The CLI's atomic update/delete path prepares plans in this module,
/// bypassing the ordinary KG handlers. Registry ownership must survive
/// both existing-row mutation and an attempted tag assignment.
#[tokio::test]
async fn atomic_entity_writes_refuse_pack_registry_tags() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut registry = khive_storage::Entity::new("local", "project", "registry-target");
    registry.tags = vec!["ToOl-ReGiStRy".into()];
    let registry_id = registry.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(registry)
        .await
        .expect("seed registry row");

    let update = prepare_update(
        &runtime,
        &token,
        &json!({"id": registry_id.to_string(), "name": "hijacked"}),
        None,
    )
    .await
    .expect_err("atomic update must reject current registry row");
    assert!(matches!(update, RuntimeError::InvalidInput(ref msg) if msg.contains("tool-registry")));

    for hard in [false, true] {
        let deletion = prepare_delete(
            &runtime,
            &token,
            &json!({"id": registry_id.to_string(), "hard": hard}),
            None,
        )
        .await
        .expect_err("atomic delete must reject current registry row");
        assert!(
            matches!(deletion, RuntimeError::InvalidInput(ref msg) if msg.contains("tool-registry"))
        );
    }
    let unchanged = runtime
        .get_entity(&token, registry_id)
        .await
        .expect("registry row remains");
    assert_eq!(unchanged.name, "registry-target");
    assert_eq!(unchanged.tags, vec!["ToOl-ReGiStRy".to_string()]);

    let plain = khive_storage::Entity::new("local", "project", "plain-target");
    let plain_id = plain.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(plain)
        .await
        .expect("seed ordinary row");
    let tagging = prepare_update(
        &runtime,
        &token,
        &json!({"id": plain_id.to_string(), "tags": ["TOOL-REGISTRY"]}),
        None,
    )
    .await
    .expect_err("atomic update must not mint a registry row");
    assert!(
        matches!(tagging, RuntimeError::InvalidInput(ref msg) if msg.contains("tool-registry"))
    );
    assert!(runtime
        .get_entity(&token, plain_id)
        .await
        .expect("ordinary row remains")
        .tags
        .is_empty());

    // Atomic merge is currently rejected at the CLI, but the public
    // prepare dispatch retains a direct merge arm. Keep it guarded too.
    for (into_id, from_id) in [(registry_id, plain_id), (plain_id, registry_id)] {
        let merge = prepare_merge(
            &runtime,
            &token,
            &json!({"into_id": into_id.to_string(), "from_id": from_id.to_string()}),
        )
        .await
        .expect_err("atomic merge must reject either protected operand");
        assert!(
            matches!(merge, RuntimeError::InvalidInput(ref msg) if msg.contains("tool-registry"))
        );
    }
}

/// Atomic `update` must reject a field that does not apply to the
/// resolved substrate: parity with
/// `khive-pack-kg::handlers::update::reject_inapplicable_fields`.
/// Without this check, atomic prepare would silently ignore `salience`
/// on an entity: it would set every entity field to its current value,
/// bump `updated_at`, satisfy the `exactly(1)` guard, and commit: a
/// spurious no-op reported as success.
#[tokio::test]
async fn atomic_update_entity_rejects_note_only_field_salience() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entity = khive_storage::Entity::new("local", "concept", "GapFourEntity");
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let err = prepare_update(
        &runtime,
        &token,
        &json!({"id": entity_id.to_string(), "salience": 0.9}),
        None,
    )
    .await
    .expect_err("salience on an entity must be rejected, not silently accepted");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("salience") && msg.contains("not valid for an entity")),
        "expected an InvalidInput naming the offending field, got: {err:?}"
    );

    // A valid entity update (name/description/tags) must still work.
    let plan = prepare_update(
        &runtime,
        &token,
        &json!({
            "id": entity_id.to_string(),
            "name": "GapFourEntity-renamed",
            "description": "updated description",
            "tags": ["a", "b"],
        }),
        None,
    )
    .await
    .expect("a valid entity field set must still be accepted");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));
    let entity = runtime
        .get_entity(&token, entity_id)
        .await
        .expect("get_entity");
    assert_eq!(entity.name, "GapFourEntity-renamed");
}

/// The `--atomic` seam intentionally requires a full UUID (never a short
/// hex prefix) for its `id` fields — prefix resolution already happened
/// upstream, at the kkernel CLI boundary that has namespace context
/// (`resolve_kg_ids_in_args`). The rejection message must say *why* a
/// prefix cannot be resolved at this stage, not just restate that a full
/// UUID is required.
#[tokio::test]
async fn atomic_update_short_prefix_id_rejected_with_namespace_explanation() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let err = prepare_update(
        &runtime,
        &token,
        &json!({"id": "deadbeef", "name": "whatever"}),
        None,
    )
    .await
    .expect_err("a short hex prefix must be rejected by the atomic-plan seam");
    let msg = match err {
        RuntimeError::InvalidInput(ref msg) => msg.clone(),
        other => panic!("expected InvalidInput, got: {other:?}"),
    };
    assert!(
        msg.contains("full UUID"),
        "message must still state the rule; got: {msg}"
    );
    assert!(
        msg.to_ascii_lowercase().contains("namespace"),
        "message must explain the namespace-scoping consequence, not just restate the \
             rule; got: {msg}"
    );
}

#[tokio::test]
async fn atomic_update_entity_type_persists_patch_and_schedules_reindex() {
    let runtime = scratch_runtime();
    runtime.install_entity_type_validator(std::sync::Arc::new(|kind, entity_type| {
        let Some(raw) = entity_type else {
            return Ok(None);
        };
        let normalized = raw.trim().to_ascii_lowercase();
        if kind == "concept" && normalized == "algorithm" {
            Ok(Some(normalized))
        } else {
            Err(RuntimeError::InvalidInput(format!(
                "unknown entity_type {raw:?} for {kind:?}; valid: algorithm"
            )))
        }
    }));
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut entity = khive_storage::Entity::new("local", "concept", "AtomicHistorical");
    entity.description = Some("keep description".to_string());
    entity.properties = Some(json!({"type": "algorithm", "keep": true}));
    entity.tags = vec!["keep-tag".to_string()];
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": entity_id.to_string(), "entity_type": " Algorithm "}),
        None,
    )
    .await
    .expect("atomic prepare must accept a registered entity_type");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("atomic update must run");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[PostCommitEffect::ReindexEntity { entity_id }],
        "entity_type patch must schedule the normal entity reindex path"
    );

    let updated = runtime
        .get_entity(&token, entity_id)
        .await
        .expect("read updated entity");
    assert_eq!(updated.entity_type.as_deref(), Some("algorithm"));
    assert_eq!(updated.name, "AtomicHistorical");
    assert_eq!(updated.description.as_deref(), Some("keep description"));
    assert_eq!(
        updated.properties,
        Some(json!({"type": "algorithm", "keep": true}))
    );
    assert_eq!(updated.tags, vec!["keep-tag"]);
}

/// Symmetric note-substrate case: `description` is entity-only and
/// must be rejected the same way update.rs rejects it.
#[tokio::test]
async fn atomic_update_note_rejects_entity_only_field_description() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut note = khive_storage::note::Note::new("local", "observation", "gap-4 note content");
    note.name = Some("gap-four-note".to_string());
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let err = prepare_update(
        &runtime,
        &token,
        &json!({"id": note_id.to_string(), "description": "entities have descriptions, notes don't"}),
            None,
        )
    .await
    .expect_err("description on a note must be rejected, not silently accepted");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("description") && msg.contains("not valid for a note")),
        "expected an InvalidInput naming the offending field, got: {err:?}"
    );

    // A valid note update (content) must still work.
    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": note_id.to_string(), "content": "gap-4 note content, revised"}),
        None,
    )
    .await
    .expect("a valid note field must still be accepted");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));
}

#[tokio::test]
async fn atomic_update_note_tags_replace_preserve_clear_and_override_nested_tags() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut note = khive_storage::note::Note::new("local", "observation", "tagged note");
    note.properties = Some(json!({"tags": ["old"], "keep": {"value": 1}}));
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed note");

    for (mut args, expected_tags) in [
        (
            json!({"tags": ["new", "shared"], "properties": {"tags": ["nested"], "added": true}}),
            json!(["new", "shared"]),
        ),
        (
            json!({"name": "renamed note", "properties": {"omitted": true}}),
            json!(["new", "shared"]),
        ),
        (
            json!({"tags": null, "properties": null}),
            json!(["new", "shared"]),
        ),
        (
            json!({"tags": [], "properties": {"tags": ["nested-after-clear"]}}),
            json!([]),
        ),
        (
            json!({"tags": ["after-null-properties"], "properties": null}),
            json!(["after-null-properties"]),
        ),
    ] {
        args["id"] = json!(note_id.to_string());
        let original_args = args.clone();
        let plan = prepare_update(&runtime, &token, &args, None)
            .await
            .expect("valid atomic note tags patch");
        assert_eq!(
            args, original_args,
            "preparation must not mutate caller args"
        );
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("atomic note update");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));
        let updated = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(note_id)
            .await
            .expect("read note")
            .expect("note exists");
        let properties = updated.properties.expect("note properties");
        assert_eq!(properties["tags"], expected_tags);
        assert_eq!(properties["keep"], json!({"value": 1}));
        assert_eq!(properties["added"], json!(true));
        assert_eq!(updated.content, "tagged note");
        if original_args.get("name").is_some() {
            assert_eq!(updated.name.as_deref(), Some("renamed note"));
            assert_eq!(properties["omitted"], json!(true));
        }
    }
}

#[tokio::test]
async fn atomic_update_entity_tags_keep_replace_preserve_and_clear_semantics() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut entity = khive_storage::Entity::new("local", "concept", "tagged entity");
    entity.tags = vec!["old".to_string()];
    entity.properties = Some(json!({"keep": true}));
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");
    for (mut args, expected_tags) in [
        (json!({"tags": ["new", "shared"]}), json!(["new", "shared"])),
        (json!({"name": "renamed entity"}), json!(["new", "shared"])),
        (json!({"tags": null}), json!(["new", "shared"])),
        (json!({"tags": []}), json!([])),
    ] {
        args["id"] = json!(entity_id.to_string());
        let plan = prepare_update(&runtime, &token, &args, None)
            .await
            .expect("valid atomic entity tags patch");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("atomic entity update");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));
        let updated = runtime
            .get_entity(&token, entity_id)
            .await
            .expect("read entity");
        assert_eq!(json!(updated.tags), expected_tags);
        assert_eq!(updated.properties, Some(json!({"keep": true})));
    }
}

#[tokio::test]
async fn atomic_update_note_invalid_tags_leave_snapshot_unchanged() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut note = khive_storage::note::Note::new("local", "observation", "unchanged note");
    note.properties = Some(json!({"tags": ["keep"], "other": true}));
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed note");
    let before = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(note_id)
        .await
        .expect("read note")
        .expect("note exists");

    for (mut args, expected_error) in [
        (
            json!({"tags": "invalid"}),
            "tags must be an array of strings",
        ),
        (
            json!({"tags": ["valid", 1]}),
            "tags must be an array of strings",
        ),
        (
            json!({"tags": {"nested": true}}),
            "tags must be an array of strings",
        ),
        (
            json!({"tags": ["valid"], "properties": []}),
            "properties must be an object",
        ),
        (
            json!({"tags": [], "properties": "invalid"}),
            "properties must be an object",
        ),
        (
            json!({"tags": "invalid", "description": "entity field"}),
            "field 'description' is not valid for a note",
        ),
    ] {
        args["id"] = json!(note_id.to_string());
        args["content"] = json!("must not persist");
        let error = prepare_update(&runtime, &token, &args, None)
            .await
            .expect_err("invalid tags patch must not produce a plan");
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message.contains(expected_error)),
            "unexpected error: {error:?}"
        );
        let after = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(note_id)
            .await
            .expect("read note")
            .expect("note exists");
        assert_eq!(after, before);
    }
}

/// Updating a note's content inside an atomic unit must, after commit,
/// leave the note recallable via FTS under its new content and its
/// vector row refreshed: parity with the non-atomic
/// `update_note` -> `reindex_note` path.
#[tokio::test]
async fn atomic_update_note_content_is_fts_and_vector_reindexed_post_commit() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let mut note = khive_storage::note::Note::new("local", "observation", "original content");
    note.name = Some("reindex-target".to_string());
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed note");

    // Sanity: no vector row yet for the stub model.
    let vec_store = runtime
        .vectors_for_model(&token, STUB_MODEL)
        .expect("vec store");
    assert_eq!(vec_store.count().await.expect("count before"), 0);

    let updated_content = format!("freshly-updated-content-xyz{}", "x".repeat(MAX_TEXT_BYTES));
    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": note_id.to_string(), "content": updated_content, "embed": true}),
        None,
    )
    .await
    .expect("prepare update");

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[PostCommitEffect::ReindexNote {
            note_id,
            version: 2
        }],
        "content change must schedule exactly one ReindexNote post-commit effect"
    );

    let embedding_outcomes = apply_post_commit_effects_with_report(&runtime, &token, post_commit)
        .await
        .expect("apply post-commit effects");
    assert_eq!(embedding_outcomes.len(), 1);
    assert_eq!(
        embedding_outcomes[0].effect,
        PostCommitEffect::ReindexNote {
            note_id,
            version: 2
        }
    );
    assert_eq!(embedding_outcomes[0].truncation.truncated, 1);
    assert!(embedding_outcomes[0].truncation.discarded_bytes > 0);

    // FTS: the note must be recallable under its NEW content.
    let doc = runtime
        .text_for_notes(&token)
        .expect("text store")
        .get_document("local", note_id)
        .await
        .expect("get_document")
        .expect("document must be indexed after post-commit reindex");
    assert!(
        doc.body.contains("freshly-updated-content-xyz"),
        "FTS body must reflect the committed content: {:?}",
        doc.body
    );

    // Vector: a row must now exist for the registered stub model.
    assert_eq!(
        vec_store.count().await.expect("count after"),
        1,
        "post-commit reindex must have inserted a vector row for the stub model"
    );
}

/// The atomic-plan path must fire the pack-installed note-mutation hook
/// for both an atomic note UPDATE (`PostCommitEffect::ReindexNote`'s
/// handler fires it after its own reindex, mirroring `update_note()`
/// on the non-atomic path) and an atomic note DELETE (`DeletePlan`
/// carries a `PostCommitEffect::NoteDeleted` that
/// `apply_post_commit_effects` dispatches directly, mirroring
/// `operations.rs::delete_note`'s direct-fire, no-refetch shape: the
/// row may already be gone by the time this runs, for a hard delete).
/// A minimal counting hook proves both fire; no `khive-pack-memory`
/// dependency is needed at this layer, since the hook itself is
/// generic.
#[tokio::test]
async fn atomic_note_update_and_delete_post_commit_effects_execute_exactly_once() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let fired: std::sync::Arc<std::sync::Mutex<Vec<(String, uuid::Uuid)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let fired_for_hook = fired.clone();
    runtime.install_note_mutation_hook(std::sync::Arc::new(move |kind: String, id: uuid::Uuid| {
        let fired = fired_for_hook.clone();
        Box::pin(async move {
            fired.lock().expect("lock").push((kind, id));
        })
    }));

    // Update path.
    let mut note = khive_storage::note::Note::new("local", "observation", "hook-update-target");
    note.name = Some("hook-update-target".to_string());
    let update_note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed update-target note");

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": update_note_id.to_string(), "content": "hook-update-target, revised"}),
        None,
    )
    .await
    .expect("prepare update");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    apply_post_commit_effects(&runtime, &token, post_commit)
        .await
        .expect("apply post-commit effects (update)");

    // Delete path (soft delete: the row still exists, but the hook
    // fires directly from the captured kind rather than refetching).
    let mut del_note = khive_storage::note::Note::new("local", "observation", "hook-delete-target");
    del_note.name = Some("hook-delete-target".to_string());
    let delete_note_id = del_note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(del_note)
        .await
        .expect("seed delete-target note");

    let plan = prepare_delete(
        &runtime,
        &token,
        &json!({"id": delete_note_id.to_string(), "hard": false}),
        None,
    )
    .await
    .expect("prepare delete");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[PostCommitEffect::NoteDeleted {
            note_id: delete_note_id,
            kind: "observation".to_string(),
        }],
        "a committed note delete must schedule exactly one NoteDeleted post-commit effect"
    );
    apply_post_commit_effects(&runtime, &token, post_commit)
        .await
        .expect("apply post-commit effects (delete)");

    assert_eq!(
        *fired.lock().expect("lock"),
        vec![
            ("observation".to_string(), update_note_id),
            ("observation".to_string(), delete_note_id),
        ],
        "each committed token must execute its note-mutation effect exactly once"
    );
}

#[tokio::test]
async fn failed_post_commit_reindexes_do_not_skip_later_note_mutation_hook() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let fired: std::sync::Arc<std::sync::Mutex<Vec<uuid::Uuid>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let fired_for_hook = fired.clone();
    runtime.install_note_mutation_hook(std::sync::Arc::new(
        move |_kind: String, id: uuid::Uuid| {
            let fired = fired_for_hook.clone();
            Box::pin(async move {
                fired.lock().expect("lock").push(id);
            })
        },
    ));

    let mut plans = Vec::new();
    let mut failed_ids = Vec::new();
    for name in ["first", "second"] {
        let note = khive_storage::note::Note::new("local", "observation", name);
        let id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed reindex target");
        plans.push(
            prepare_update(
                &runtime,
                &token,
                // Explicit embedding is required here: with embed=None
                // and no existing vector row, atomic_runner legitimately
                // replaces ReindexNote with NoteChanged (inheritance
                // policy), leaving no reindex for the fault to fail.
                &json!({"id": id.to_string(), "content": format!("{name} revised"), "embed": true}),
                None,
            )
            .await
            .expect("prepare note update"),
        );
        failed_ids.push(id);
    }
    let deleted = khive_storage::note::Note::new("local", "observation", "deleted");
    let deleted_id = deleted.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(deleted)
        .await
        .expect("seed delete target");
    plans.push(
        prepare_delete(
            &runtime,
            &token,
            &json!({"id": deleted_id.to_string(), "hard": false}),
            None,
        )
        .await
        .expect("prepare note delete"),
    );

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), plans)
        .await
        .expect("commit atomic unit");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[
            PostCommitEffect::ReindexNote {
                note_id: failed_ids[0],
                version: 2,
            },
            PostCommitEffect::ReindexNote {
                note_id: failed_ids[1],
                version: 2,
            },
            PostCommitEffect::NoteDeleted {
                note_id: deleted_id,
                kind: "observation".into(),
            },
        ],
        "the fixture must commit two reindexes before the deletion hook"
    );

    // The already-committed note rows remain intact, but both deferred
    // reindexes now fail. Both current-note hooks and the later delete fire.
    let mut writer = runtime.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "DROP TABLE fts_notes".into(),
            params: Vec::new(),
            label: Some("test_remove_fts_notes_after_commit".into()),
        })
        .await
        .expect("remove FTS table");
    drop(writer);

    let error = apply_post_commit_effects_with_report(&runtime, &token, post_commit)
        .await
        .expect_err("both reindexes must fail")
        .to_string();
    assert!(error.contains("effect[0]"), "{error}");
    assert!(error.contains("effect[1]"), "{error}");
    for id in &failed_ids {
        assert!(error.contains(&id.to_string()), "{error}");
    }
    failed_ids.push(deleted_id);
    assert_eq!(*fired.lock().expect("lock"), failed_ids);
}

/// Atomic delete must purge the note's FTS row and vector row for both
/// soft and hard delete: parity with `KhiveRuntime::delete_note`'s
/// index-cleanup contract.
#[tokio::test]
async fn atomic_delete_note_purges_fts_and_vector_indexes_soft_and_hard() {
    use khive_storage::types::VectorRecord;

    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    for hard in [false, true] {
        let mut note =
            khive_storage::note::Note::new("local", "observation", "purge-target content");
        note.name = Some(format!("purge-target-hard-{hard}"));
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note.clone())
            .await
            .expect("seed note");
        runtime
            .reindex_note(&token, &note)
            .await
            .expect("seed index rows");

        let vec_store = runtime
            .vectors_for_model(&token, STUB_MODEL)
            .expect("vec store");
        vec_store
            .insert_batch(vec![VectorRecord {
                subject_id: note_id,
                kind: SubstrateKind::Note,
                namespace: "local".into(),
                field: "note.content".into(),
                embedding_model: Some(STUB_MODEL.into()),
                vectors: vec![vec![0.5_f32; STUB_DIMS]],
                text_fingerprint: Some(VectorRecord::fingerprint_text("purge-target content")),
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("seed attributed vector");
        assert!(vec_store
            .provenance(note_id)
            .await
            .unwrap()
            .unwrap()
            .text_fingerprint
            .is_some());
        assert_eq!(
            vec_store.count().await.expect("count before"),
            1,
            "seeded note must have a vector row before delete (hard={hard})"
        );
        assert!(
            runtime
                .text_for_notes(&token)
                .expect("text store")
                .get_document("local", note_id)
                .await
                .expect("get_document")
                .is_some(),
            "seeded note must have an FTS row before delete (hard={hard})"
        );

        let plan = prepare_delete(
            &runtime,
            &token,
            &json!({"id": note_id.to_string(), "hard": hard}),
            None,
        )
        .await
        .expect("prepare delete");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected commit (hard={hard}): {outcome:?}"
        );

        assert!(
            runtime
                .text_for_notes(&token)
                .expect("text store")
                .get_document("local", note_id)
                .await
                .expect("get_document")
                .is_none(),
            "FTS row must be purged after atomic delete (hard={hard})"
        );
        assert_eq!(
            vec_store.count().await.expect("count after"),
            0,
            "vector row must be purged after atomic delete (hard={hard})"
        );
        let mut reader = runtime.sql().reader().await.expect("sql reader");
        let sidecar_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM vector_provenance WHERE subject_id = ?1".into(),
                params: vec![SqlValue::Text(note_id.to_string())],
                label: Some("test-atomic-delete-sidecar-clear".into()),
            })
            .await
            .expect("count sidecar rows");
        assert!(matches!(sidecar_count, Some(SqlValue::Integer(0))));
    }
}

/// An explicit embed=false note update uses NoteVectors::apply inside the
/// writer transaction. It must physically remove that note's sidecar row
/// along with its vec0 row while retaining another note's provenance.
#[tokio::test]
async fn note_vectors_embed_false_purge_clears_provenance_across_models() {
    use khive_storage::types::VectorRecord;

    const FOREIGN_MODEL: &str = "stub-adr044-a4-foreign";
    struct ForeignStubProvider;
    #[async_trait]
    impl EmbedderProvider for ForeignStubProvider {
        fn name(&self) -> &str {
            FOREIGN_MODEL
        }

        fn dimensions(&self) -> usize {
            STUB_DIMS
        }

        async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(StubService))
        }
    }

    async fn provenance_count(runtime: &KhiveRuntime, subject_id: Uuid, model: &str) -> i64 {
        let mut reader = runtime.sql().reader().await.expect("sql reader");
        let count = reader
            .query_scalar(crate::note_write::statement(
                "SELECT COUNT(*) FROM vector_provenance \
                     WHERE model_key=?1 AND namespace=?2 AND subject_id=?3",
                vec![
                    SqlValue::Text(crate::config::sanitize_key(model)),
                    SqlValue::Text("local".into()),
                    SqlValue::Text(subject_id.to_string()),
                ],
            ))
            .await
            .expect("read physical provenance row count");
        let Some(SqlValue::Integer(count)) = count else {
            panic!("expected physical provenance count, got {count:?}");
        };
        count
    }

    async fn ann_delete_count(runtime: &KhiveRuntime, subject_id: Uuid, model: &str) -> i64 {
        let mut reader = runtime.sql().reader().await.expect("sql reader");
        let count = reader
            .query_scalar(crate::note_write::statement(
                "SELECT COUNT(*) FROM ann_write_log \
                     WHERE namespace=?1 AND embedding_model=?2 AND subject_id=?3 AND op='delete'",
                vec![
                    SqlValue::Text("local".into()),
                    SqlValue::Text(model.into()),
                    SqlValue::Text(subject_id.to_string()),
                ],
            ))
            .await
            .expect("read ANN delete log");
        let Some(SqlValue::Integer(count)) = count else {
            panic!("expected ANN delete count, got {count:?}");
        };
        count
    }

    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    runtime.register_embedder(SecondStubProvider);
    runtime.register_embedder(ForeignStubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut ids = Vec::new();
    for content in ["purge provenance", "keep provenance"] {
        let note = khive_storage::note::Note::new("local", "observation", content);
        ids.push(note.id);
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note.clone())
            .await
            .expect("seed note");
        runtime
            .reindex_note(&token, &note)
            .await
            .expect("seed note vector");
        // Custom providers correctly produce no attested fingerprint. Seed
        // a known historical attribution for both live vectors so this
        // test can observe whether the raw purge physically clears it.
        for model in [STUB_MODEL, SECOND_STUB_MODEL] {
            runtime
                .vectors_for_model(&token, model)
                .expect("vec store")
                .insert_batch(vec![VectorRecord {
                    subject_id: note.id,
                    kind: khive_types::SubstrateKind::Note,
                    namespace: note.namespace.clone(),
                    field: "note.content".into(),
                    embedding_model: Some(model.into()),
                    vectors: vec![vec![0.5; STUB_DIMS]],
                    text_fingerprint: Some(VectorRecord::fingerprint_text(&note.content)),
                    updated_at: chrono::Utc::now(),
                }])
                .await
                .expect("seed known historical attribution");
        }
    }
    let [purged_id, retained_id] = [ids[0], ids[1]];
    runtime
        .vectors_for_model(&token, FOREIGN_MODEL)
        .expect("foreign-model vec store")
        .insert_batch(vec![VectorRecord {
            subject_id: purged_id,
            kind: khive_types::SubstrateKind::Note,
            namespace: "foreign".into(),
            field: "note.content".into(),
            embedding_model: Some(FOREIGN_MODEL.into()),
            vectors: vec![vec![0.5; STUB_DIMS]],
            text_fingerprint: Some(VectorRecord::fingerprint_text("foreign source")),
            updated_at: chrono::Utc::now(),
        }])
        .await
        .expect("seed same-subject foreign vec0 and sidecar");
    let foreign_count = |runtime: &KhiveRuntime| {
        let writer = runtime
            .backend()
            .pool()
            .try_writer()
            .expect("fixture writer");
        let conn = writer.conn();
        let model_key = crate::config::sanitize_key(FOREIGN_MODEL);
        let sidecar: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM vector_provenance WHERE model_key=?1 AND namespace='foreign' AND subject_id=?2",
                rusqlite::params![model_key, purged_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let vector: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM vec_{model_key} WHERE namespace='foreign' AND subject_id=?1"),
                [purged_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        (sidecar, vector)
    };
    assert_eq!(foreign_count(&runtime), (1, 1));
    for model in [STUB_MODEL, SECOND_STUB_MODEL] {
        assert_eq!(provenance_count(&runtime, purged_id, model).await, 1);
        assert_eq!(provenance_count(&runtime, retained_id, model).await, 1);
    }
    // Replacing the foreign model's local seed row logs a delete before
    // this purge. Check the purge's delta rather than the lifetime total.
    let delete_logs_before = [
        ann_delete_count(&runtime, purged_id, STUB_MODEL).await,
        ann_delete_count(&runtime, purged_id, SECOND_STUB_MODEL).await,
    ];
    let foreign_delete_logs_before = ann_delete_count(&runtime, purged_id, FOREIGN_MODEL).await;

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": purged_id.to_string(), "embed": false}),
        None,
    )
    .await
    .expect("prepare explicit vector purge");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("run atomic update");
    assert!(
        matches!(
            &outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "expected committed update, got {outcome:?}"
    );

    for model in [STUB_MODEL, SECOND_STUB_MODEL] {
        assert_eq!(provenance_count(&runtime, purged_id, model).await, 0);
        assert_eq!(provenance_count(&runtime, retained_id, model).await, 1);
        assert_eq!(
            runtime
                .vectors_for_model(&token, model)
                .expect("vec store")
                .count()
                .await
                .expect("vector row count"),
            1,
            "only the retained note's vector row remains for {model}"
        );
    }
    assert_eq!(foreign_count(&runtime), (1, 1));
    for (model, before) in [STUB_MODEL, SECOND_STUB_MODEL]
        .into_iter()
        .zip(delete_logs_before)
    {
        assert_eq!(
            ann_delete_count(&runtime, purged_id, model).await,
            before + 1,
            "purge must log exactly one delete for {model}"
        );
    }
    assert_eq!(
        ann_delete_count(&runtime, purged_id, FOREIGN_MODEL).await,
        foreign_delete_logs_before,
        "purge must not log a delete for the foreign namespace"
    );
}

/// A failure while purging the second model must roll back the first
/// model's vec0 DELETE, sidecar DELETE, and ANN-log insert as one unit.
#[tokio::test]
async fn note_vectors_second_model_sidecar_failure_rolls_back_all_models() {
    use khive_storage::types::VectorRecord;

    #[derive(Debug, PartialEq, Eq)]
    struct PersistedVector {
        embedding_hex: String,
        digest: String,
        fingerprint: Option<String>,
        updated_at: Option<String>,
        ann_delete_count: i64,
    }

    fn persisted(runtime: &KhiveRuntime, model: &str, subject_id: Uuid) -> PersistedVector {
        let key = crate::config::sanitize_key(model);
        let writer = runtime.backend().pool().try_writer().expect("pool writer");
        let conn = writer.conn();
        let embedding_hex = conn
            .query_row(
                &format!(
                    "SELECT hex(embedding) FROM vec_{key} \
                         WHERE namespace=?1 AND subject_id=?2"
                ),
                rusqlite::params!["local", subject_id.to_string()],
                |row| row.get(0),
            )
            .expect("read persisted vec0 embedding");
        let (digest, fingerprint, updated_at) = conn
            .query_row(
                "SELECT embedding_digest, text_fingerprint, updated_at \
                     FROM vector_provenance \
                     WHERE model_key=?1 AND namespace=?2 AND subject_id=?3",
                rusqlite::params![key, "local", subject_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read persisted model sidecar");
        let ann_delete_count = conn
            .query_row(
                "SELECT COUNT(*) FROM ann_write_log \
                     WHERE namespace=?1 AND embedding_model=?2 \
                     AND subject_id=?3 AND op='delete'",
                rusqlite::params!["local", model, subject_id.to_string()],
                |row| row.get(0),
            )
            .expect("read persisted ANN delete count");
        PersistedVector {
            embedding_hex,
            digest,
            fingerprint,
            updated_at,
            ann_delete_count,
        }
    }

    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    runtime.register_embedder(SecondStubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let note = khive_storage::note::Note::new("local", "observation", "rollback provenance");
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note.clone())
        .await
        .expect("seed note");
    runtime
        .reindex_note(&token, &note)
        .await
        .expect("seed note vectors");
    for model in [STUB_MODEL, SECOND_STUB_MODEL] {
        runtime
            .vectors_for_model(&token, model)
            .expect("vec store")
            .insert_batch(vec![VectorRecord {
                subject_id: note.id,
                kind: khive_types::SubstrateKind::Note,
                namespace: note.namespace.clone(),
                field: "note.content".into(),
                embedding_model: Some(model.into()),
                vectors: vec![vec![0.5; STUB_DIMS]],
                text_fingerprint: Some(VectorRecord::fingerprint_text(&note.content)),
                updated_at: chrono::Utc::now(),
            }])
            .await
            .expect("seed known historical attribution");
    }
    let before = [
        persisted(&runtime, STUB_MODEL, note.id),
        persisted(&runtime, SECOND_STUB_MODEL, note.id),
    ];

    // NoteVectors::tables orders the vec_* catalog by name. Abort only
    // the second model's sidecar DELETE, after the first model has already
    // logged and deleted its vector and sidecar inside this transaction.
    let mut keys = [
        crate::config::sanitize_key(STUB_MODEL),
        crate::config::sanitize_key(SECOND_STUB_MODEL),
    ];
    keys.sort();
    {
        let writer = runtime.backend().pool().try_writer().expect("pool writer");
        writer
            .conn()
            .execute_batch(&format!(
                "CREATE TRIGGER fail_second_note_vector_sidecar_delete \
                     BEFORE DELETE ON vector_provenance \
                     WHEN OLD.model_key='{}' AND OLD.namespace='local' \
                      AND OLD.subject_id='{}' \
                     BEGIN SELECT RAISE(ABORT, 'injected second-model sidecar delete failure'); END;",
                keys[1], note.id
            ))
            .expect("install second-model sidecar fault");
    }

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": note.id.to_string(), "embed": false}),
        None,
    )
    .await
    .expect("prepare explicit vector purge");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("atomic seam should report an op rollback");
    match outcome {
        crate::atomic_runner::AtomicRunOutcome::RolledBack {
            failed_op_index: 0,
            failure:
                crate::atomic_runner::AtomicOpFailure::SqlError {
                    statement_label,
                    message,
                },
        } => {
            assert_eq!(statement_label.as_deref(), Some("note-vector-purge"));
            assert!(
                message.contains("injected second-model sidecar delete failure"),
                "unexpected rollback cause: {message}"
            );
        }
        other => panic!("expected the second-model purge to roll back, got {other:?}"),
    }

    assert_eq!(persisted(&runtime, STUB_MODEL, note.id), before[0]);
    assert_eq!(persisted(&runtime, SECOND_STUB_MODEL, note.id), before[1]);
    let stored = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(note.id)
        .await
        .expect("read note")
        .expect("note remains");
    assert_eq!(stored.version, note.version, "note update also rolled back");
}

/// Atomic delete must purge the entity's FTS row and vector row for
/// both soft and hard delete: parity with
/// `KhiveRuntime::delete_entity`'s index-cleanup contract.
#[tokio::test]
async fn atomic_delete_entity_purges_fts_and_vector_indexes_soft_and_hard() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    for hard in [false, true] {
        let entity =
            khive_storage::Entity::new("local", "concept", format!("purge-target-hard-{hard}"));
        let entity_id = entity.id;
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(entity.clone())
            .await
            .expect("seed entity");
        runtime
            .reindex_entity(&token, &entity)
            .await
            .expect("seed index rows");

        let vec_store = runtime
            .vectors_for_model(&token, STUB_MODEL)
            .expect("vec store");
        assert_eq!(
            vec_store.count().await.expect("count before"),
            1,
            "seeded entity must have a vector row before delete (hard={hard})"
        );
        assert!(
            runtime
                .text(&token)
                .expect("text store")
                .get_document("local", entity_id)
                .await
                .expect("get_document")
                .is_some(),
            "seeded entity must have an FTS row before delete (hard={hard})"
        );

        let plan = prepare_delete(
            &runtime,
            &token,
            &json!({"id": entity_id.to_string(), "hard": hard}),
            None,
        )
        .await
        .expect("prepare delete");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected commit (hard={hard}): {outcome:?}"
        );

        assert!(
            runtime
                .text(&token)
                .expect("text store")
                .get_document("local", entity_id)
                .await
                .expect("get_document")
                .is_none(),
            "FTS row must be purged after atomic delete (hard={hard})"
        );
        assert_eq!(
            vec_store.count().await.expect("count after"),
            0,
            "vector row must be purged after atomic delete (hard={hard})"
        );
    }
}

#[tokio::test]
async fn atomic_delete_entity_and_note_logs_vector_delete_rows() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let entity = khive_storage::Entity::new("local", "concept", "ann-delete-entity");
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity.clone())
        .await
        .expect("seed entity");
    runtime
        .reindex_entity(&token, &entity)
        .await
        .expect("seed entity vector row");
    let note = khive_storage::note::Note::new("local", "observation", "ann-delete-note");
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note.clone())
        .await
        .expect("seed note");
    runtime
        .reindex_note(&token, &note)
        .await
        .expect("seed note vector row");

    for id in [entity_id, note_id] {
        let plan = prepare_delete(&runtime, &token, &json!({"id": id.to_string()}), None)
            .await
            .expect("prepare delete");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));
    }

    let mut reader = runtime.sql().reader().await.expect("sql reader");
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log \
                      WHERE namespace = 'local' AND embedding_model = ?1 AND op = 'delete' \
                      AND ((kind = 'entity' AND field = 'entity.body' AND subject_id = ?2) \
                        OR (kind = 'note' AND field = 'note.content' AND subject_id = ?3))"
                .to_string(),
            params: vec![
                SqlValue::Text(STUB_MODEL.to_string()),
                SqlValue::Text(entity_id.to_string()),
                SqlValue::Text(note_id.to_string()),
            ],
            label: Some("test-atomic-delete-ann-write-log".to_string()),
        })
        .await
        .expect("query ANN write log");
    assert!(matches!(count, Some(SqlValue::Integer(2))));
}

/// Atomic prepare validates the requested orientation before canonicalizing
/// a symmetric edge for persistence. Fixed UUIDs force target < source so a
/// regression would report the reverse ordered pair.
#[tokio::test]
async fn atomic_link_symmetric_rejection_preserves_requested_pair() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let concept_id = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").expect("high UUID");
    let project_id = Uuid::nil();
    assert!(project_id < concept_id, "test must exercise UUID reversal");

    let mut concept = khive_storage::Entity::new("local", "concept", "Concept source");
    concept.id = concept_id;
    let mut project = khive_storage::Entity::new("local", "project", "Project target");
    project.id = project_id;
    entities.upsert_entity(concept).await.expect("seed concept");
    entities.upsert_entity(project).await.expect("seed project");

    let error = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": concept_id.to_string(),
            "target_id": project_id.to_string(),
            "relation": "competes_with",
        }),
    )
    .await
    .expect_err("atomic link must reject concept competes_with project");
    let message = error.to_string();
    assert!(
        message.contains(
            "currently legal relations for concept -> project under the loaded endpoint rules: none"
        ),
        "atomic validation must diagnose caller order before persistence canonicalization; got: {message}"
    );
    assert!(
        !message.contains("currently legal relations for project -> concept"),
        "atomic validation must not diagnose the UUID-canonical reverse pair; got: {message}"
    );
}

/// Atomic link must persist an explicit top-level `dependency_kind`
/// param into edge metadata, and must infer one for `depends_on` edges
/// when absent: parity with
/// `link.rs`'s `merge_entry_metadata` and `operations.rs`'s
/// `infer_dependency_kind` table.
#[tokio::test]
async fn atomic_link_persists_explicit_dependency_kind_and_infers_when_absent() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");

    fn metadata_json(plan: &AtomicOpPlan) -> String {
        let link_plan = match plan {
            AtomicOpPlan::Link(p) => p,
            other => panic!("expected an AtomicOpPlan::Link, got {other:?}"),
        };
        match link_plan.statements[0].statement.params.last() {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("expected the metadata param to be SqlValue::Text, got {other:?}"),
        }
    }

    // (a) explicit top-level `dependency_kind` param persists in metadata.
    {
        let svc = khive_storage::Entity::new("local", "service", "SvcA");
        let proj = khive_storage::Entity::new("local", "project", "ProjB");
        let (svc_id, proj_id) = (svc.id, proj.id);
        entities.upsert_entity(svc).await.expect("seed svc");
        entities.upsert_entity(proj).await.expect("seed proj");

        let plan = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": svc_id.to_string(),
                "target_id": proj_id.to_string(),
                "relation": "depends_on",
                "dependency_kind": "artifact",
            }),
        )
        .await
        .expect("prepare link");
        let json_str = metadata_json(&plan);
        assert!(
            json_str.contains(r#""dependency_kind":"artifact""#),
            "explicit dependency_kind param must persist: {json_str}"
        );
    }

    // (b) `depends_on` with no explicit dependency_kind infers from
    // endpoint kinds: (service, service) -> "runtime".
    {
        let svc_a = khive_storage::Entity::new("local", "service", "SvcC");
        let svc_b = khive_storage::Entity::new("local", "service", "SvcD");
        let (a_id, b_id) = (svc_a.id, svc_b.id);
        entities.upsert_entity(svc_a).await.expect("seed svc a");
        entities.upsert_entity(svc_b).await.expect("seed svc b");

        let plan = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": a_id.to_string(),
                "target_id": b_id.to_string(),
                "relation": "depends_on",
            }),
        )
        .await
        .expect("prepare link");
        let json_str = metadata_json(&plan);
        assert!(
            json_str.contains(r#""dependency_kind":"runtime""#),
            "inferred dependency_kind for (service, service) must persist: {json_str}"
        );
    }
}

#[tokio::test]
async fn atomic_link_rejects_malformed_metadata_before_inference() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let left = khive_storage::Entity::new("local", "document", "Left");
    let right = khive_storage::Entity::new("local", "document", "Right");
    let (left_id, right_id) = (left.id, right.id);
    entities.upsert_entity(left).await.expect("seed left");
    entities.upsert_entity(right).await.expect("seed right");

    for metadata in [json!(false), json!({"optional": "false"})] {
        let error = prepare_link(
            &runtime,
            &token,
            &json!({
                "source_id": left_id.to_string(),
                "target_id": right_id.to_string(),
                "relation": "depends_on",
                "metadata": metadata,
            }),
        )
        .await
        .expect_err("malformed metadata cannot produce a link plan");
        assert!(format!("{error}").contains("metadata"), "{error}");
    }
}

#[tokio::test]
async fn atomic_entity_kind_hint_checks_subtype_before_mutation() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut entity = khive_storage::Entity::new("local", "document", "Typed Document");
    entity.entity_type = Some("paper".to_string());
    let id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let wrong_update = prepare_update(
        &runtime,
        &token,
        &json!({"id": id.to_string(), "name": "Wrong Type"}),
        Some(AtomicUpdateKind::Entity {
            specific: Some("document".to_string()),
            entity_type: Some("report".to_string()),
        }),
    )
    .await
    .expect_err("subtype mismatch must refuse atomic update");
    assert!(matches!(wrong_update, RuntimeError::NotFound(_)));

    let wrong_delete = prepare_delete(
        &runtime,
        &token,
        &json!({"id": id.to_string()}),
        Some(AtomicDeleteKind::Entity {
            specific: Some("document".to_string()),
            entity_type: Some("report".to_string()),
        }),
    )
    .await
    .expect_err("subtype mismatch must refuse atomic delete");
    assert!(matches!(wrong_delete, RuntimeError::NotFound(_)));

    let changed_type = prepare_update(
        &runtime,
        &token,
        &json!({"id": id.to_string(), "entity_type": "report"}),
        Some(AtomicUpdateKind::Entity {
            specific: Some("document".to_string()),
            entity_type: Some("paper".to_string()),
        }),
    )
    .await
    .expect_err("subtype-qualified update cannot change subtype");
    assert!(format!("{changed_type}").contains("contradicts"));
}

#[tokio::test]
async fn atomic_update_subtype_hint_accepts_legacy_null_but_refuses_conflicts() {
    let runtime = scratch_runtime();
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let legacy = khive_storage::Entity::new("local", "document", "Legacy Document");
    let legacy_id = legacy.id;
    let mut typed = khive_storage::Entity::new("local", "document", "Typed Report");
    typed.entity_type = Some("report".into());
    let typed_id = typed.id;
    let entities = runtime.entities(&token).expect("entities store");
    entities
        .upsert_entity(legacy)
        .await
        .expect("seed legacy row");
    entities.upsert_entity(typed).await.expect("seed typed row");

    let kind = || AtomicUpdateKind::Entity {
        specific: Some("document".into()),
        entity_type: Some("paper".into()),
    };
    let wrong_base = prepare_update(
        &runtime,
        &token,
        &json!({"id": legacy_id.to_string(), "name": "Wrong Base"}),
        Some(AtomicUpdateKind::Entity {
            specific: Some("artifact".into()),
            entity_type: Some("paper".into()),
        }),
    )
    .await
    .expect_err("legacy NULL subtype cannot override a conflicting base kind");
    assert!(matches!(wrong_base, RuntimeError::NotFound(_)));
    let wrong_type = prepare_update(
        &runtime,
        &token,
        &json!({"id": typed_id.to_string(), "name": "Wrong Type"}),
        Some(kind()),
    )
    .await
    .expect_err("stored non-NULL report must refuse a paper hint");
    assert!(matches!(wrong_type, RuntimeError::NotFound(_)));
    let clear = prepare_update(
        &runtime,
        &token,
        &json!({"id": legacy_id.to_string(), "entity_type": null}),
        Some(kind()),
    )
    .await
    .expect_err("an explicit clear still contradicts a paper hint");
    assert!(matches!(clear, RuntimeError::InvalidInput(_)));

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": legacy_id.to_string(), "name": "Renamed Legacy Document"}),
        Some(kind()),
    )
    .await
    .expect("matching base kind permits a paper hint on a legacy NULL row");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit legacy update");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));
    let updated = runtime
        .get_entity(&token, legacy_id)
        .await
        .expect("read updated row");
    assert_eq!(updated.name, "Renamed Legacy Document");
    assert_eq!(updated.entity_type, None);
}

#[tokio::test]
async fn atomic_delete_subtype_hint_accepts_legacy_null_but_refuses_conflicts() {
    let runtime = scratch_runtime();
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let legacy = khive_storage::Entity::new("local", "document", "Legacy Deletion");
    let legacy_id = legacy.id;
    let mut typed = khive_storage::Entity::new("local", "document", "Typed Deletion");
    typed.entity_type = Some("report".into());
    let typed_id = typed.id;
    let entities = runtime.entities(&token).expect("entities store");
    entities
        .upsert_entity(legacy)
        .await
        .expect("seed legacy row");
    entities.upsert_entity(typed).await.expect("seed typed row");

    let kind = || AtomicDeleteKind::Entity {
        specific: Some("document".into()),
        entity_type: Some("paper".into()),
    };
    let wrong_base = prepare_delete(
        &runtime,
        &token,
        &json!({"id": legacy_id.to_string()}),
        Some(AtomicDeleteKind::Entity {
            specific: Some("artifact".into()),
            entity_type: Some("paper".into()),
        }),
    )
    .await
    .expect_err("legacy NULL subtype cannot override a conflicting base kind");
    assert!(matches!(wrong_base, RuntimeError::NotFound(_)));
    let wrong_type = prepare_delete(
        &runtime,
        &token,
        &json!({"id": typed_id.to_string()}),
        Some(kind()),
    )
    .await
    .expect_err("stored non-NULL report must refuse a paper hint");
    assert!(matches!(wrong_type, RuntimeError::NotFound(_)));

    let plan = prepare_delete(
        &runtime,
        &token,
        &json!({"id": legacy_id.to_string()}),
        Some(kind()),
    )
    .await
    .expect("matching base kind permits a paper hint on a legacy NULL row");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit legacy delete");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));
    assert!(runtime
        .resolve_by_id(&token, legacy_id)
        .await
        .expect("resolve deleted row")
        .is_none());
}

/// Raw natural-key probe of `graph_edges` (namespace, source_id,
/// target_id, relation) — returns `(weight, metadata_json, deleted_at)`
/// for exactly the ONE row a UNIQUE(namespace, source_id, target_id,
/// relation) constraint permits. `None` means no row at all.
async fn probe_edge_natural_key(
    runtime: &KhiveRuntime,
    namespace: &str,
    source_id: Uuid,
    target_id: Uuid,
    relation: &str,
) -> (usize, Option<f64>, Option<String>, Option<i64>) {
    let mut reader = runtime.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT weight, metadata, deleted_at FROM graph_edges \
                      WHERE namespace = ?1 AND source_id = ?2 AND target_id = ?3 AND relation = ?4"
                .to_string(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(source_id.to_string()),
                SqlValue::Text(target_id.to_string()),
                SqlValue::Text(relation.to_string()),
            ],
            label: Some("test-probe-edge-natural-key".to_string()),
        })
        .await
        .expect("probe edge natural key");
    let count = rows.len();
    let Some(row) = rows.into_iter().next() else {
        return (count, None, None, None);
    };
    let weight = match row.get("weight") {
        Some(SqlValue::Float(f)) => Some(*f),
        Some(SqlValue::Integer(i)) => Some(*i as f64),
        _ => None,
    };
    let metadata = match row.get("metadata") {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let deleted_at = match row.get("deleted_at") {
        Some(SqlValue::Integer(i)) => Some(*i),
        _ => None,
    };
    (count, weight, metadata, deleted_at)
}

/// Atomic `link` must be an upsert, exactly like canonical `link` ->
/// `upsert_edge`'s natural-key `ON CONFLICT` arm: re-linking an
/// already-linked triple must succeed and update weight/metadata, not
/// hit the `UNIQUE(namespace, source_id, target_id, relation)`
/// constraint and roll back the whole atomic unit.
#[tokio::test]
async fn atomic_link_of_already_linked_triple_upserts_weight_and_metadata() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");

    let a = khive_storage::Entity::new("local", "concept", "GapTwoA");
    let b = khive_storage::Entity::new("local", "concept", "GapTwoB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    // (a) a fresh link still works.
    let plan1 = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": a_id.to_string(),
            "target_id": b_id.to_string(),
            "relation": "extends",
            "weight": 0.5,
        }),
    )
    .await
    .expect("prepare first link");
    let outcome1 = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan1])
        .await
        .expect("seam call ok");
    assert!(
        matches!(
            outcome1,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "fresh link must commit: {outcome1:?}"
    );
    let (count, weight, _metadata, deleted_at) =
        probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
    assert_eq!(count, 1, "exactly one edge row after the fresh link");
    assert_eq!(weight, Some(0.5));
    assert!(deleted_at.is_none());

    // (b) re-linking the SAME triple with a different weight/metadata
    // must SUCCEED (not a constraint-violation rollback) and UPDATE the
    // existing row in place — natural key stays unique.
    let plan2 = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": a_id.to_string(),
            "target_id": b_id.to_string(),
            "relation": "extends",
            "weight": 0.9,
            "metadata": {"note": "relinked"},
        }),
    )
    .await
    .expect("prepare second link");
    let outcome2 = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan2])
        .await
        .expect("seam call ok");
    assert!(
        matches!(
            outcome2,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "re-link of an already-linked triple must upsert, not roll back: {outcome2:?}"
    );
    let (count, weight, metadata, deleted_at) =
        probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
    assert_eq!(
        count, 1,
        "the natural-key UNIQUE constraint must still hold exactly one row (upsert, not a second insert)"
    );
    assert_eq!(weight, Some(0.9), "weight must be updated to the new value");
    assert!(
        metadata
            .as_deref()
            .is_some_and(|m| m.contains(r#""note":"relinked""#)),
        "metadata must be updated to the new value: {metadata:?}"
    );
    assert!(deleted_at.is_none());
}

/// Atomic `link` refuses a soft-deleted triple by default and only
/// resurrects it when the caller opts in explicitly.
#[tokio::test]
async fn atomic_link_of_soft_deleted_triple_resurrects_it() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");

    let a = khive_storage::Entity::new("local", "concept", "GapTwoResurrectA");
    let b = khive_storage::Entity::new("local", "concept", "GapTwoResurrectB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    let plan = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": a_id.to_string(),
            "target_id": b_id.to_string(),
            "relation": "extends",
        }),
    )
    .await
    .expect("prepare link");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));

    // Soft-delete the edge row directly (natural-key UPDATE — mirrors
    // what `delete_edge(hard=false)` does to this same row).
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE graph_edges SET deleted_at = ?1 \
                          WHERE namespace = ?2 AND source_id = ?3 AND target_id = ?4 AND relation = ?5"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
                    SqlValue::Text("local".to_string()),
                    SqlValue::Text(a_id.to_string()),
                    SqlValue::Text(b_id.to_string()),
                    SqlValue::Text("extends".to_string()),
                ],
                label: Some("test-soft-delete-edge".to_string()),
            })
            .await
            .expect("soft delete edge");
        assert_eq!(affected, 1, "soft-delete must touch exactly the seeded row");
    }
    let (_, _, _, deleted_at) =
        probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
    assert!(
        deleted_at.is_some(),
        "row must be soft-deleted before the resurrect attempt"
    );

    let refusal = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": a_id.to_string(),
            "target_id": b_id.to_string(),
            "relation": "extends",
            "weight": 0.75,
        }),
    )
    .await
    .expect_err("implicit resurrection must be refused at prepare time");
    assert!(matches!(
        refusal,
        RuntimeError::InvalidInput(message) if message.contains("resurrect=true")
    ));
    let (_, weight, _, deleted_at) =
        probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
    assert_eq!(weight, Some(1.0), "refusal must preserve the tombstone");
    assert!(deleted_at.is_some());

    let plan_relink = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": a_id.to_string(),
            "target_id": b_id.to_string(),
            "relation": "extends",
            "weight": 0.75,
            "resurrect": true,
        }),
    )
    .await
    .expect("prepare explicitly resurrecting link");
    let outcome_relink =
        crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan_relink])
            .await
            .expect("seam call ok");
    assert!(
        matches!(
            outcome_relink,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "re-linking a soft-deleted triple must resurrect it, not roll back: {outcome_relink:?}"
    );
    let (count, weight, _, deleted_at) =
        probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
    assert_eq!(count, 1);
    assert_eq!(weight, Some(0.75));
    assert!(
        deleted_at.is_none(),
        "re-link must resurrect the soft-deleted row (deleted_at -> NULL)"
    );
}

/// Atomic delete of an entity and a note must succeed even when the
/// registered embedding model's `vec_*` table has never been lazily
/// created (a fresh DB registers models before any vector store is
/// opened): the raw purge DML must skip tables that don't exist
/// rather than hit `no such table` and roll back the whole atomic
/// unit. FTS purge still fires (those tables always exist) and the
/// delete itself is a clean commit.
#[tokio::test]
async fn atomic_delete_succeeds_when_vec_table_never_created() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    // Seed via raw upsert ONLY — never call reindex_entity/reindex_note
    // or vectors_for_model, so the stub model's `vec_*` table is never
    // lazily created (opening the vector store is what creates it).
    let entity = khive_storage::Entity::new("local", "concept", "no-vec-table-entity");
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let mut note = khive_storage::note::Note::new("local", "observation", "no-vec-table-note");
    note.name = Some("no-vec-table-note".to_string());
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed note");

    for (id, kind) in [(entity_id, "entity"), (note_id, "note")] {
        let plan = prepare_delete(&runtime, &token, &json!({"id": id.to_string()}), None)
            .await
            .unwrap_or_else(|e| panic!("prepare delete ({kind}) must not fail: {e}"));
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap_or_else(|e| panic!("atomic delete ({kind}) must not hit `no such table`: {e}"));
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean commit ({kind}): {outcome:?}"
        );
    }

    assert!(
        runtime
            .get_entity_including_deleted(&token, entity_id)
            .await
            .expect("get entity")
            .expect("entity row still present (soft delete)")
            .deleted_at
            .is_some(),
        "entity must be soft-deleted"
    );
    assert!(
        runtime
            .get_note_including_deleted(&token, note_id)
            .await
            .expect("get note")
            .expect("note row still present (soft delete)")
            .deleted_at
            .is_some(),
        "note must be soft-deleted"
    );
}

/// Atomic hard delete must be able to purge a record that was already
/// soft-deleted: parity with `delete(id, hard=true)` being the public
/// purge route after a prior soft delete (the non-atomic hard path
/// resolves including deleted rows and its DML carries no `deleted_at`
/// predicate).
#[tokio::test]
async fn atomic_hard_delete_purges_already_soft_deleted_entity_and_note() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let entity = khive_storage::Entity::new("local", "concept", "tombstoned-entity-hard-delete");
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity.clone())
        .await
        .expect("seed entity");
    runtime
        .reindex_entity(&token, &entity)
        .await
        .expect("seed index rows");

    let mut note =
        khive_storage::note::Note::new("local", "observation", "tombstoned-note-hard-delete");
    note.name = Some("tombstoned-note-hard-delete".to_string());
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note.clone())
        .await
        .expect("seed note");
    runtime
        .reindex_note(&token, &note)
        .await
        .expect("seed index rows");

    // First: SOFT delete both (via atomic prepare) so they're tombstoned
    // going into the hard-delete attempt below.
    for id in [entity_id, note_id] {
        let plan = prepare_delete(&runtime, &token, &json!({"id": id.to_string()}), None)
            .await
            .expect("prepare soft delete");
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .expect("soft delete commit");
        assert!(matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ));
    }
    assert!(
        runtime
            .get_entity_including_deleted(&token, entity_id)
            .await
            .expect("get entity")
            .expect("entity present")
            .deleted_at
            .is_some(),
        "entity must be soft-deleted before the hard-delete attempt"
    );
    assert!(
        runtime
            .get_note_including_deleted(&token, note_id)
            .await
            .expect("get note")
            .expect("note present")
            .deleted_at
            .is_some(),
        "note must be soft-deleted before the hard-delete attempt"
    );

    // Now: HARD delete the already-tombstoned records.
    for (id, kind) in [(entity_id, "entity"), (note_id, "note")] {
        let plan = prepare_delete(
            &runtime,
            &token,
            &json!({"id": id.to_string(), "hard": true}),
            None,
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "prepare hard delete ({kind}) of an already-soft-deleted record \
                         must resolve it: {e}"
            )
        });
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap_or_else(|e| panic!("hard delete ({kind}) commit failed: {e}"));
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean hard-delete commit ({kind}): {outcome:?}"
        );
    }

    assert!(
        runtime
            .get_entity_including_deleted(&token, entity_id)
            .await
            .expect("get entity")
            .is_none(),
        "entity row must be fully purged after hard delete"
    );
    assert!(
        runtime
            .get_note_including_deleted(&token, note_id)
            .await
            .expect("get note")
            .is_none(),
        "note row must be fully purged after hard delete"
    );
    assert!(
        runtime
            .text(&token)
            .expect("text store")
            .get_document("local", entity_id)
            .await
            .expect("get_document")
            .is_none(),
        "entity FTS row must be purged after hard delete"
    );
    assert!(
        runtime
            .text_for_notes(&token)
            .expect("text store")
            .get_document("local", note_id)
            .await
            .expect("get_document")
            .is_none(),
        "note FTS row must be purged after hard delete"
    );
    let vec_store = runtime
        .vectors_for_model(&token, STUB_MODEL)
        .expect("vec store");
    assert_eq!(
        vec_store.count().await.expect("count after"),
        0,
        "vector rows for both records must be purged after hard delete"
    );
}

// ------------------------------------------------------------------
// event-store append parity
// ------------------------------------------------------------------

/// Fetch every event of `kind` targeting `target_id`, via the same
/// `EventStore::query_events` surface `--atomic` callers would use to
/// verify parity — not a raw SQL probe.
async fn events_for_target(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    target_id: Uuid,
    kind: EventKind,
) -> Vec<khive_storage::Event> {
    let event_store = runtime.events(token).expect("event store");
    let filter = khive_storage::EventFilter {
        kinds: vec![kind],
        ..Default::default()
    };
    let page = event_store
        .query_events(filter, khive_storage::types::PageRequest::default())
        .await
        .expect("query_events");
    page.items
        .into_iter()
        .filter(|e| e.target_id == Some(target_id))
        .collect()
}

/// Atomic `update(id=<entity>, name=...)` must append an
/// `EntityUpdated` event, matching `curation::update_entity`: the
/// event is appended unconditionally after a successful row update,
/// not only on the reindex-triggering subset.
#[tokio::test]
async fn atomic_update_entity_appends_entity_updated_event() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entity = khive_storage::Entity::new("local", "concept", "gap1-entity");
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": entity_id.to_string(), "name": "gap1-entity-renamed"}),
        None,
    )
    .await
    .expect("prepare update");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));

    let events = events_for_target(&runtime, &token, entity_id, EventKind::EntityUpdated).await;
    assert_eq!(
        events.len(),
        1,
        "expected exactly one EntityUpdated event for {entity_id}"
    );
    assert_eq!(events[0].namespace, "local");
    assert_eq!(
        events[0].actor, "anonymous:local",
        "atomic event attribution must come from the authorized token"
    );
    assert_eq!(events[0].payload["id"], json!(entity_id.to_string()));
    assert_eq!(
        events[0].payload["changed_fields"],
        json!(["name"]),
        "changed_fields must name exactly the patched fields"
    );
}

/// Atomic soft and hard delete of an entity must each append an
/// `EntityDeleted` event, matching `operations::delete_entity`, which
/// fires on both delete modes.
#[tokio::test]
async fn atomic_delete_entity_appends_entity_deleted_event_soft_and_hard() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    for hard in [false, true] {
        let entity =
            khive_storage::Entity::new("local", "concept", format!("gap1-entity-hard-{hard}"));
        let entity_id = entity.id;
        runtime
            .entities(&token)
            .expect("entities store")
            .upsert_entity(entity)
            .await
            .expect("seed entity");

        let args = if hard {
            json!({"id": entity_id.to_string(), "hard": true})
        } else {
            json!({"id": entity_id.to_string()})
        };
        let plan = prepare_delete(&runtime, &token, &args, None)
            .await
            .unwrap_or_else(|e| panic!("prepare delete (hard={hard}): {e}"));
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap_or_else(|e| panic!("delete commit (hard={hard}): {e}"));
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean delete commit (hard={hard}): {outcome:?}"
        );

        let events = events_for_target(&runtime, &token, entity_id, EventKind::EntityDeleted).await;
        assert_eq!(
            events.len(),
            1,
            "expected exactly one EntityDeleted event for {entity_id} (hard={hard})"
        );
        assert_eq!(events[0].payload["hard"], json!(hard));
    }
}

#[tokio::test]
async fn atomic_hard_delete_emits_lineage_warning_in_commit_unit() {
    let runtime = scratch_runtime();
    let token = NamespaceToken::mint_authorized(
        Namespace::local(),
        crate::ActorRef::new("agent", "atomic-lineage-deleter"),
    );
    let doomed = khive_storage::Entity::new("local", "document", "atomic-doomed");
    let source = khive_storage::Entity::new("local", "artifact", "atomic-source");
    let doomed_id = doomed.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(doomed)
        .await
        .expect("seed doomed entity");
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(source.clone())
        .await
        .expect("seed source entity");
    runtime
        .link(
            &token,
            source.id,
            doomed_id,
            EdgeRelation::DerivedFrom,
            1.0,
            None,
        )
        .await
        .expect("seed protected edge");

    let plan = prepare_delete(
        &runtime,
        &token,
        &json!({"id": doomed_id.to_string(), "hard": true}),
        None,
    )
    .await
    .expect("prepare hard delete");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("delete commit");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));

    let warnings = events_for_target(&runtime, &token, doomed_id, EventKind::Audit).await;
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].actor, "agent:atomic-lineage-deleter");
    assert_eq!(warnings[0].payload["relation"], "derived_from");
    assert_eq!(warnings[0].payload["warning"], "provenance_loss");
}

/// Atomic soft and hard delete of a note must each append a
/// `NoteDeleted` event, matching `operations::delete_note`, which
/// fires on both delete modes.
#[tokio::test]
async fn atomic_delete_note_appends_note_deleted_event_soft_and_hard() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    for hard in [false, true] {
        let mut note = khive_storage::note::Note::new(
            "local",
            "observation",
            format!("gap1-note-content-hard-{hard}"),
        );
        note.name = Some(format!("gap1-note-hard-{hard}"));
        let note_id = note.id;
        runtime
            .notes(&token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed note");

        let args = if hard {
            json!({"id": note_id.to_string(), "hard": true})
        } else {
            json!({"id": note_id.to_string()})
        };
        let plan = prepare_delete(&runtime, &token, &args, None)
            .await
            .unwrap_or_else(|e| panic!("prepare delete (hard={hard}): {e}"));
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap_or_else(|e| panic!("delete commit (hard={hard}): {e}"));
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean delete commit (hard={hard}): {outcome:?}"
        );

        let events = events_for_target(&runtime, &token, note_id, EventKind::NoteDeleted).await;
        assert_eq!(
            events.len(),
            1,
            "expected exactly one NoteDeleted event for {note_id} (hard={hard})"
        );
        assert_eq!(events[0].payload["hard"], json!(hard));
    }
}

/// `update` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`; this
/// asserts `prepare_update` actually builds a plan for one, a
/// non-symmetric relation (`extends`) exercises the
/// `edge_upsert_statement` reuse branch, and that the committed row +
/// `EdgeUpdated` event match canonical `update_edge`'s shape (weight
/// persisted, relation unchanged, exactly one event).
#[tokio::test]
async fn atomic_update_edge_patches_weight_and_appends_edge_updated_event() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "GapEdgeA");
    let b = khive_storage::Entity::new("local", "concept", "GapEdgeB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    let edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.4, None)
        .await
        .expect("seed edge");
    let edge_id = Uuid::from(edge.id);

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": edge_id.to_string(), "weight": 0.75}),
        None,
    )
    .await
    .expect("prepare update edge");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(
        matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "expected a clean edge update commit: {outcome:?}"
    );

    let updated = runtime
        .get_edge(&token, edge_id)
        .await
        .expect("get_edge")
        .expect("edge still present");
    assert_eq!(updated.weight, 0.75, "weight patch must persist");
    assert_eq!(updated.relation, EdgeRelation::Extends);

    let events = events_for_target(&runtime, &token, edge_id, EventKind::EdgeUpdated).await;
    assert_eq!(
        events.len(),
        1,
        "expected exactly one EdgeUpdated event for {edge_id}"
    );
    assert_eq!(
        events[0].payload["changed_fields"],
        json!(["weight"]),
        "changed_fields must name exactly the patched field"
    );
}

/// ADR-115 Amendment 1 §3: the reserved `khive:secret_gate` property key
/// must be rejected on atomic edge-metadata updates the same way it is
/// on canonical `update_edge`.
#[tokio::test]
async fn atomic_update_edge_rejects_reserved_secret_gate_property() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "ReservedEdgeA");
    let b = khive_storage::Entity::new("local", "concept", "ReservedEdgeB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    let edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.4, None)
        .await
        .expect("seed edge");
    let edge_id = Uuid::from(edge.id);

    let err = prepare_update(
        &runtime,
        &token,
        &json!({
            "id": edge_id.to_string(),
            "properties": {"khive:secret_gate": "exempted:content-sha256-manifest-v1"},
        }),
        None,
    )
    .await
    .expect_err("a caller-supplied reserved key on edge metadata must be rejected");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate") && msg.contains("runtime-owned")),
        "expected a reservation rejection, got: {err:?}"
    );

    let unchanged = runtime
        .get_edge(&token, edge_id)
        .await
        .expect("get_edge")
        .expect("edge still present");
    assert!(
        unchanged.metadata.is_none(),
        "rejected edge update must leave metadata untouched"
    );
}

/// The symmetric-relation conflict-absorption branch of
/// `prepare_update_edge` — mirrors `update_edge_symmetric_dml`'s case
/// (b): changing a non-symmetric edge's `relation` to a symmetric one
/// whose canonical natural key collides with an ALREADY-EXISTING
/// symmetric edge between the same two entities must delete the
/// requested (non-canonical) row and leave the surviving canonical row
/// untouched (ADR-039 ON CONFLICT DO NOTHING), rather than raising a
/// uniqueness error OR overwriting the survivor with the discarded
/// edge's attributes (khive#1213).
#[tokio::test]
async fn atomic_update_edge_symmetric_conflict_absorbs_into_surviving_row() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "GapEdgeSymA");
    let b = khive_storage::Entity::new("local", "concept", "GapEdgeSymB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    // The non-canonical edge under test: A -> B, non-symmetric relation.
    let requested_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
        .await
        .expect("seed requested edge");
    let requested_id = Uuid::from(requested_edge.id);

    // The pre-existing canonical row this update will collide with once
    // `relation` becomes `competes_with` (symmetric).
    let canonical_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::CompetesWith, 0.6, None)
        .await
        .expect("seed canonical edge");
    let canonical_id = Uuid::from(canonical_edge.id);
    assert_ne!(requested_id, canonical_id);

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": requested_id.to_string(), "relation": "competes_with", "weight": 0.9}),
        None,
    )
    .await
    .expect("prepare update edge (symmetric conflict)");
    // The plan does not compute a prepare-time advisory surviving id
    // (`target_id` is just the requested id): it carries
    // `edge_natural_key` so a post-commit caller can derive the real
    // surviving id itself. Assert the plan carries the right natural
    // key to look up; the actual surviving row's identity is verified
    // against the DB after commit, below.
    let (canon_src, canon_tgt) = canonical_edge_endpoints(EdgeRelation::CompetesWith, a_id, b_id);
    match &plan {
        AtomicOpPlan::Update(p) => {
            assert_eq!(p.target_id, requested_id);
            let key = p
                .edge_natural_key
                .as_ref()
                .expect("symmetric edge update must carry edge_natural_key");
            assert_eq!(key.canon_source_id, canon_src);
            assert_eq!(key.canon_target_id, canon_tgt);
            assert_eq!(key.relation, EdgeRelation::CompetesWith);
        }
        other => panic!("expected an Update plan, got {other:?}"),
    }

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(
        matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "expected a clean symmetric-conflict-absorption commit: {outcome:?}"
    );

    // The requested (non-canonical) row must be gone.
    let requested_after = runtime
        .get_edge_including_deleted(&token, requested_id)
        .await
        .expect("get_edge_including_deleted");
    assert!(
        requested_after.is_none(),
        "the non-canonical requested row must be deleted, not just tombstoned"
    );

    // ADR-039 DO NOTHING: the surviving canonical row keeps its OWN
    // pre-existing attributes — the discarded edge's patched weight
    // (0.9) must never overwrite it.
    let surviving = runtime
        .get_edge(&token, canonical_id)
        .await
        .expect("get_edge")
        .expect("surviving canonical row must remain");
    assert_eq!(
        surviving.weight, 0.6,
        "survivor weight must not be overwritten by the discarded edge's patch"
    );
    assert_eq!(surviving.relation, EdgeRelation::CompetesWith);

    // Event target is the CALLER-supplied id, not the surviving id —
    // mirrors `update_edge`'s event using `edge_id` (the caller's
    // original argument), not the post-absorption id.
    let events = events_for_target(&runtime, &token, requested_id, EventKind::EdgeUpdated).await;
    assert_eq!(events.len(), 1);
}

/// Regression for khive #1753: an entity atomic update plan is prepared
/// from one revision, a concurrent (out-of-plan) writer advances the row
/// past that revision before the plan commits, and the plan's guarded
/// `entity_replace_if_unchanged_statement` must then affect zero rows —
/// which `AffectedRowGuard::exactly(1)` must turn into a whole-unit
/// rollback, not a silent overwrite of the concurrent writer's change.
/// Before threading the expected revision into the plan's `WHERE`
/// predicate, this plan used the unconditional `entity_upsert_statement`,
/// which always affects exactly 1 row regardless of staleness — the
/// guard could never fire and this test would redden (the outcome would
/// be `Committed`, and the concurrent writer's `name` change would be
/// lost under the stale plan's `description`-only patch).
#[tokio::test]
async fn atomic_entity_update_plan_stale_revision_rolls_back_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entity = runtime
        .create_entity(
            &token,
            "concept",
            None,
            "StaleEntityPlanTarget",
            None,
            Some(json!({"a": 0})),
            vec![],
        )
        .await
        .expect("seed entity");
    let id = entity.id;

    // PREPARE time: build the plan from the current (soon-to-be-stale)
    // revision.
    let mut plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": id.to_string(), "description": "from the stale plan"}),
        None,
    )
    .await
    .expect("prepare update entity plan");

    // Read the plan's OWN bound values, so the isolation below is
    // derived from the plan rather than assumed about it. Per
    // `entity_replace_if_unchanged_statement`, `?8` (index 7) is the
    // replacement revision, `?12` (index 11) the target id, `?13`
    // (index 12) the expected revision, and `?14` (index 13) the
    // expected deletion marker.
    let (planned_replacement, plan_target_id, plan_expected, plan_expected_deleted) = {
        let statements = match &plan {
            AtomicOpPlan::Update(p) => p.statements.clone(),
            other => panic!("expected an Update plan, got {other:?}"),
        };
        // Locate by LABEL, never by the guard text. A locator keyed on
        // `?8 > updated_at` would stop finding the statement in exactly the
        // mutation run that deletes that conjunct, so the arm would report
        // on this fixture's locator instead of on the guard.
        let cas = statements
            .iter()
            .find(|s| s.statement.label.as_deref() == Some("entity-replace-if-unchanged"))
            .expect("the plan must carry the guarded entity replacement");
        let read = |i: usize| match &cas.statement.params[i] {
            SqlValue::Integer(v) => *v,
            other => panic!("param {i} must be an integer revision, got {other:?}"),
        };
        let read_marker = |i: usize| match &cas.statement.params[i] {
            SqlValue::Null => None,
            SqlValue::Integer(v) => Some(*v),
            other => panic!("param {i} must be a deletion marker, got {other:?}"),
        };
        let read_text = |i: usize| match &cas.statement.params[i] {
            SqlValue::Text(v) => v.clone(),
            other => panic!("param {i} must be a text id, got {other:?}"),
        };
        (read(7), read_text(11), read(12), read_marker(13))
    };
    // The identity conjunct, on the same footing as the revision and the
    // deletion marker. `id = ?12` is live in the same UPDATE, so a plan
    // that bound any other row's id would affect zero rows and roll the
    // unit back for a reason this test does not name — an outcome
    // indistinguishable from the one it does name.
    assert_eq!(
        plan_target_id,
        id.to_string(),
        "fixture premise: the plan's `?12` must be the row under test, otherwise \
             `id = ?12` refuses on identity and the rollback stops being attributable to \
             the expected-revision guard"
    );

    // A concurrent writer commits BEFORE the plan runs, advancing the
    // row's revision past what the plan's guard expects.
    runtime
        .update_entity(
            &token,
            id,
            crate::curation::EntityPatch {
                name: Some("ConcurrentWriterWon".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("concurrent writer update");

    // Pin the stored revision one microsecond BELOW the plan's replacement.
    // This is what makes the test specific to the guard it names. Both the
    // plan and the concurrent writer derive their revision from
    // `max(now, expected + 1)`, so left alone the concurrent write lands at
    // or past the plan's own replacement and BOTH conjuncts refuse — the
    // test would then stay green if either guard were deleted. Pinning
    // leaves `?8 > updated_at` satisfied, so it cannot be the refuser,
    // while `updated_at = ?13` is violated, which is the single condition
    // this test names. The shape is reachable in production whenever the
    // concurrent writer's clock trails the preparing writer's.
    let stored_pinned = planned_replacement - 1;
    assert_ne!(
        stored_pinned, plan_expected,
        "fixture premise: the pinned revision must differ from the plan's expected \
             revision, otherwise `updated_at = ?13` would MATCH and nothing would refuse \
             the plan"
    );
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE entities SET version = version + 1, updated_at = ?1 WHERE id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(stored_pinned),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test-pin-stored-revision".to_string()),
            })
            .await
            .expect("pin the stored revision");
        assert_eq!(affected, 1, "the pin must touch exactly the seeded row");
    }
    assert!(
        planned_replacement > stored_pinned,
        "fixture premise: the plan's replacement must still strictly advance past the \
             stored revision, otherwise `?8 > updated_at` would refuse and this stops being \
             a test of the expected-revision guard"
    );
    // The remaining non-target conjunct. `deleted_at IS ?14` is live in the
    // same UPDATE, so without this read the fixture would stay green if the
    // deletion marker were the actual refuser — the refusal would look
    // identical. Read the row as it stands at DML time, after both the
    // concurrent writer and the pin.
    {
        let stored = runtime
            .get_entity_including_deleted(&token, id)
            .await
            .expect("read the stored row")
            .expect("the seeded row is present before the plan runs");
        assert_eq!(
            stored.updated_at, stored_pinned,
            "fixture premise: the pin must be what the guard reads, so the stored \
                 revision is the pinned value and nothing re-advanced it"
        );
        assert_eq!(
            stored.deleted_at, plan_expected_deleted,
            "fixture premise: the stored deletion marker must MATCH the plan's `?14`, \
                 otherwise `deleted_at IS ?14` refuses too and this stops being a test of \
                 the expected-revision guard alone"
        );
        // Keep this legacy timestamp-conjunct oracle independent of the
        // newly added persisted-version predicate. The production plan
        // would also refuse on its old version; this fixture pins only
        // that additional predicate to the observed current value.
        let AtomicOpPlan::Update(update) = &mut plan else {
            unreachable!()
        };
        let cas = update
            .statements
            .iter_mut()
            .find(|s| s.statement.label.as_deref() == Some("entity-replace-if-unchanged"))
            .unwrap();
        cas.statement.params[14] = SqlValue::Integer(stored.version);
    }

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("the seam call itself must not error; the unit rolls back cleanly");
    match outcome {
        crate::atomic_runner::AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(
                failed_op_index, 0,
                "the sole op's guard must be the one that fails"
            );
            // Attribution rather than inference. The premises above
            // establish that every non-target conjunct holds; this names
            // the statement that actually refused. Without it a unit that
            // rolled back at some other statement reads identically from
            // the outside, which is the whole failure mode those premises
            // were approximating.
            assert_eq!(
                failure,
                crate::atomic_runner::AtomicOpFailure::GuardFailed {
                    statement_label: Some("entity-replace-if-unchanged".to_string()),
                    expected: crate::atomic_plan::AffectedRowGuard::exactly(1),
                    observed: 0,
                },
                "the guarded entity replacement must be the statement whose guard refused"
            );
        }
        other => panic!(
            "a stale entity plan must roll back, not silently overwrite the concurrent \
                 writer's change: {other:?}"
        ),
    }

    let after = runtime.get_entity(&token, id).await.expect("get_entity");
    assert_eq!(
        after.name, "ConcurrentWriterWon",
        "the concurrent writer's committed name must survive the rolled-back stale plan"
    );
    assert_eq!(
        after.description, None,
        "the stale plan's description patch must NOT have landed"
    );
}

/// Edge counterpart of `atomic_entity_update_plan_stale_revision_rolls_back_unit`:
/// a non-symmetric edge atomic update plan prepared from one revision
/// must roll back the whole unit when a concurrent writer advances the
/// row first, rather than silently overwriting that writer's change.
#[tokio::test]
async fn atomic_edge_update_plan_stale_revision_rolls_back_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "StaleEdgePlanA");
    let b = khive_storage::Entity::new("local", "concept", "StaleEdgePlanB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    let edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
        .await
        .expect("seed edge");
    let edge_id = Uuid::from(edge.id);

    // PREPARE time: build the plan from the current (soon-to-be-stale)
    // revision.
    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": edge_id.to_string(), "properties": {"note": "from the stale plan"}}),
        None,
    )
    .await
    .expect("prepare update edge plan");

    // Read the plan's OWN bound values, so the isolation below is
    // derived from the plan rather than assumed about it. Per
    // `edge_replace_if_unchanged_statement`, `?6` (index 5) is the
    // replacement revision, `?10` (index 9) the target id, `?11`
    // (index 10) the expected revision, and `?12` (index 11) the expected
    // deletion marker.
    let (planned_replacement, plan_target_id, plan_expected, plan_expected_deleted) = {
        let statements = match &plan {
            AtomicOpPlan::Update(p) => p.statements.clone(),
            other => panic!("expected an Update plan, got {other:?}"),
        };
        // Locate by LABEL, never by the guard text — see the entity sibling
        // above: a locator keyed on the conjunct disappears in exactly the
        // mutation run that deletes it.
        let cas = statements
            .iter()
            .find(|s| s.statement.label.as_deref() == Some("edge-replace-if-unchanged"))
            .expect("the plan must carry the guarded edge replacement");
        let read = |i: usize| match &cas.statement.params[i] {
            SqlValue::Integer(v) => *v,
            other => panic!("param {i} must be an integer revision, got {other:?}"),
        };
        let read_marker = |i: usize| match &cas.statement.params[i] {
            SqlValue::Null => None,
            SqlValue::Integer(v) => Some(*v),
            other => panic!("param {i} must be a deletion marker, got {other:?}"),
        };
        let read_text = |i: usize| match &cas.statement.params[i] {
            SqlValue::Text(v) => v.clone(),
            other => panic!("param {i} must be a text id, got {other:?}"),
        };
        (read(5), read_text(9), read(10), read_marker(11))
    };
    // The identity conjunct, for the same reason as the entity sibling:
    // `id = ?10` is live in the same UPDATE and a misbound target refuses
    // indistinguishably.
    assert_eq!(
        plan_target_id,
        edge_id.to_string(),
        "fixture premise: the plan's `?10` must be the edge under test, otherwise \
             `id = ?10` refuses on identity and the rollback stops being attributable to \
             the expected-revision guard"
    );

    // A concurrent writer commits BEFORE the plan runs, advancing the
    // row's revision past what the plan's guard expects.
    runtime
        .update_edge(
            &token,
            edge_id,
            crate::curation::EdgePatch {
                weight: Some(0.75),
                ..Default::default()
            },
        )
        .await
        .expect("concurrent writer update");

    // Pin the stored revision one microsecond BELOW the plan's replacement,
    // for the same reason as the entity sibling above: both the plan and
    // the concurrent writer derive their revision from
    // `max(now, expected + 1)`, so left alone BOTH conjuncts refuse and the
    // test would stay green if either guard were deleted. Pinning leaves
    // `?6 > updated_at` satisfied, so it cannot be the refuser, while
    // `updated_at = ?11` is violated — the single condition this test names.
    let stored_pinned = planned_replacement - 1;
    assert_ne!(
        stored_pinned, plan_expected,
        "fixture premise: the pinned revision must differ from the plan's expected \
             revision, otherwise `updated_at = ?11` would MATCH and nothing would refuse \
             the plan"
    );
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE graph_edges SET updated_at = ?1 WHERE id = ?2".to_string(),
                params: vec![
                    SqlValue::Integer(stored_pinned),
                    SqlValue::Text(edge_id.to_string()),
                ],
                label: Some("test-pin-stored-revision".to_string()),
            })
            .await
            .expect("pin the stored revision");
        assert_eq!(affected, 1, "the pin must touch exactly the seeded edge");
    }
    assert!(
        planned_replacement > stored_pinned,
        "fixture premise: the plan's replacement must still strictly advance past the \
             stored revision, otherwise `?6 > updated_at` would refuse and this stops being \
             a test of the expected-revision guard"
    );
    // The remaining non-target conjunct, for the same reason as the entity
    // sibling: `deleted_at IS ?12` is live in the same UPDATE and would
    // produce an indistinguishable refusal.
    {
        let stored = runtime
            .get_edge_including_deleted(&token, edge_id)
            .await
            .expect("read the stored edge")
            .expect("the seeded edge is present before the plan runs");
        assert_eq!(
            stored.updated_at.timestamp_micros(),
            stored_pinned,
            "fixture premise: the pin must be what the guard reads, so the stored \
                 revision is the pinned value and nothing re-advanced it"
        );
        assert_eq!(
            stored.deleted_at.map(|d| d.timestamp_micros()),
            plan_expected_deleted,
            "fixture premise: the stored deletion marker must MATCH the plan's `?12`, \
                 otherwise `deleted_at IS ?12` refuses too and this stops being a test of \
                 the expected-revision guard alone"
        );
    }

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("the seam call itself must not error; the unit rolls back cleanly");
    match outcome {
        crate::atomic_runner::AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(
                failed_op_index, 0,
                "the sole op's guard must be the one that fails"
            );
            // Attribution rather than inference — see the entity sibling.
            assert_eq!(
                failure,
                crate::atomic_runner::AtomicOpFailure::GuardFailed {
                    statement_label: Some("edge-replace-if-unchanged".to_string()),
                    expected: crate::atomic_plan::AffectedRowGuard::exactly(1),
                    observed: 0,
                },
                "the guarded edge replacement must be the statement whose guard refused"
            );
        }
        other => panic!(
            "a stale edge plan must roll back, not silently overwrite the concurrent \
                 writer's change: {other:?}"
        ),
    }

    let after = runtime
        .get_edge(&token, edge_id)
        .await
        .expect("get_edge")
        .expect("edge still exists");
    assert!(
        (after.weight - 0.75).abs() < 0.001,
        "the concurrent writer's committed weight must survive the rolled-back stale plan: {after:?}"
    );
    assert!(
        after.metadata.is_none(),
        "the stale plan's properties patch must NOT have landed: {after:?}"
    );
}

/// A soft-deleted surviving canonical row must not be resurrected by a
/// conflicting symmetric-relation update (ADR-039 DO NOTHING; khive#1213):
/// the requested edge is still deleted (conflict absorbed), but the
/// tombstoned survivor must stay tombstoned.
#[tokio::test]
async fn atomic_update_edge_symmetric_conflict_does_not_resurrect_tombstoned_survivor() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "GapEdgeSymTombA");
    let b = khive_storage::Entity::new("local", "concept", "GapEdgeSymTombB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    let requested_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
        .await
        .expect("seed requested edge");
    let requested_id = Uuid::from(requested_edge.id);

    let canonical_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::CompetesWith, 0.6, None)
        .await
        .expect("seed canonical edge");
    let canonical_id = Uuid::from(canonical_edge.id);
    runtime
        .delete_edge(&token, canonical_id, false)
        .await
        .expect("soft-delete canonical edge");

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": requested_id.to_string(), "relation": "competes_with", "weight": 0.9}),
        None,
    )
    .await
    .expect("prepare update edge (symmetric conflict over tombstone)");

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(
        matches!(
            outcome,
            crate::atomic_runner::AtomicRunOutcome::Committed { .. }
        ),
        "expected a clean symmetric-conflict-absorption commit: {outcome:?}"
    );

    let requested_after = runtime
        .get_edge_including_deleted(&token, requested_id)
        .await
        .expect("get_edge_including_deleted");
    assert!(
        requested_after.is_none(),
        "the non-canonical requested row must be deleted, not just tombstoned"
    );

    let canonical_after = runtime
        .get_edge(&token, canonical_id)
        .await
        .expect("get_edge");
    assert!(
        canonical_after.is_none(),
        "a tombstoned survivor must not be resurrected by a conflicting update"
    );
}

/// The same-unit race: `[delete(X), update(X -> competes_with)]` where
/// an already-existing canonical row sits at the post-update natural
/// key. Both ops' async prepare passes run before either commits, so at
/// prepare time `X` still exists and both plans build. At commit time
/// `delete(X)` removes it first; `update(X -> competes_with)`'s own
/// commit-time statements must then fail loud (its target no longer
/// exists) rather than silently absorbing into the pre-existing
/// canonical row it never causally touched. The whole atomic unit must
/// roll back — parity with canonical `update_edge`'s `NotFound` for a
/// missing edge, expressed here as the unit-level abort for any op
/// whose commit-time guard fails.
#[tokio::test]
async fn atomic_update_edge_symmetric_same_unit_delete_race_aborts_the_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "GapEdgeRaceA");
    let b = khive_storage::Entity::new("local", "concept", "GapEdgeRaceB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    // The row op 1 will try to update — deleted by op 0 in the SAME unit.
    let requested_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
        .await
        .expect("seed requested edge");
    let requested_id = Uuid::from(requested_edge.id);

    // The pre-existing canonical row the buggy `id = ?2 OR natural-key`
    // predicate used to silently absorb into.
    let canonical_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::CompetesWith, 0.6, None)
        .await
        .expect("seed canonical edge");
    let canonical_id = Uuid::from(canonical_edge.id);

    let delete_plan = prepare_delete(
        &runtime,
        &token,
        &json!({"id": requested_id.to_string(), "hard": true}),
        None,
    )
    .await
    .expect("prepare delete edge");
    let update_plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": requested_id.to_string(), "relation": "competes_with", "weight": 0.9}),
        None,
    )
    .await
    .expect("prepare update edge (both prepares run before either commits)");

    let outcome = crate::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![delete_plan, update_plan],
    )
    .await
    .expect("the seam call itself must not error — the unit rolls back cleanly");
    match outcome {
        crate::atomic_runner::AtomicRunOutcome::RolledBack {
            failed_op_index, ..
        } => {
            assert_eq!(
                failed_op_index, 1,
                "op 1 (the update) must be the one whose guard fails"
            );
        }
        other => panic!("expected the whole unit to roll back, got {other:?}"),
    }

    // Whole-unit rollback: op 0's delete must be undone too.
    let requested_after = runtime
        .get_edge(&token, requested_id)
        .await
        .expect("get_edge");
    assert!(
        requested_after.is_some(),
        "delete(X) must have rolled back along with the failed update"
    );
    // The pre-existing canonical row must be completely untouched.
    let canonical_after = runtime
        .get_edge(&token, canonical_id)
        .await
        .expect("get_edge")
        .expect("canonical row must still be present");
    assert_eq!(
        canonical_after.weight, 0.6,
        "the pre-existing canonical row must never have been touched by the aborted update"
    );
}

/// The symmetric absorption delete's atomic commit-time statement
/// (`edge_symmetric_delete_if_conflict_statement`) must refuse a plan
/// built from a since-changed snapshot, exactly like the non-symmetric
/// `atomic_edge_update_plan_stale_revision_rolls_back_unit` case above —
/// even though a genuine canonical survivor exists at the target natural
/// key. `E`'s own revision changes (via an unrelated production
/// `update_edge` call) AFTER `prepare_update` captured its snapshot but
/// BEFORE the plan runs; the whole atomic unit must roll back rather than
/// silently absorbing `E` into the survivor and discarding the
/// concurrent write.
#[tokio::test]
async fn atomic_update_edge_symmetric_absorption_plan_stale_revision_rolls_back_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "StaleAbsorbA");
    let b = khive_storage::Entity::new("local", "concept", "StaleAbsorbB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    // The pre-existing canonical row this plan will try to absorb into.
    let canonical_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::CompetesWith, 0.6, None)
        .await
        .expect("seed canonical edge");
    let canonical_id = Uuid::from(canonical_edge.id);

    // E: the requested edge under test.
    let requested_edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.2, None)
        .await
        .expect("seed requested edge");
    let requested_id = Uuid::from(requested_edge.id);

    // PREPARE time: build the plan from the current (soon-to-be-stale)
    // revision.
    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": requested_id.to_string(), "relation": "competes_with", "weight": 0.9}),
        None,
    )
    .await
    .expect("prepare update edge (symmetric absorption)");

    // A concurrent writer commits BEFORE the plan runs, advancing E's
    // revision past what the plan's guard expects. Relation stays
    // `extends` (non-symmetric), so this lands via the already-guarded
    // replace path — independent of the absorption bug under test.
    let concurrent = runtime
        .update_edge(
            &token,
            requested_id,
            crate::curation::EdgePatch {
                weight: Some(0.77),
                ..Default::default()
            },
        )
        .await
        .expect("concurrent writer update");
    assert!((concurrent.weight - 0.77).abs() < 1e-9);

    // Statement 2's in-place arm carries a strict-advance conjunct
    // (`?7 > updated_at`) beside the expected-revision conjunct
    // (`updated_at = ?10`) this test names. Both the plan and the
    // concurrent writer derive their revision from `max(now, expected + 1)`
    // and the concurrent writer ran LATER, so left alone the stored
    // revision sits at or past the plan's replacement and BOTH conjuncts
    // refuse — the test would stay green with either one deleted. Pin the
    // stored revision one microsecond below the plan's replacement, the
    // same treatment the entity and edge siblings above already carry.
    let (absorb_replacement, absorb_expected, absorb_expected_deleted, absorb_ns, absorb_id) = {
        let statements = match &plan {
            AtomicOpPlan::Update(p) => p.statements.clone(),
            other => panic!("expected an Update plan, got {other:?}"),
        };
        let s = statements
            .iter()
            .find(|st| {
                st.statement.label.as_deref() == Some("edge-symmetric-absorb-or-update-inplace")
            })
            .expect("the plan must carry the guarded symmetric in-place absorb");
        let int = |i: usize| match &s.statement.params[i] {
            SqlValue::Integer(v) => *v,
            other => panic!("param {i} must be an integer revision, got {other:?}"),
        };
        let marker = |i: usize| match &s.statement.params[i] {
            SqlValue::Null => None,
            SqlValue::Integer(v) => Some(*v),
            other => panic!("param {i} must be a deletion marker, got {other:?}"),
        };
        let txt = |i: usize| match &s.statement.params[i] {
            SqlValue::Text(v) => v.clone(),
            other => panic!("param {i} must be text, got {other:?}"),
        };
        (int(6), int(9), marker(10), txt(0), txt(1))
    };
    let stored_pinned = absorb_replacement - 1;
    assert_ne!(
        stored_pinned, absorb_expected,
        "fixture premise: the pinned revision must differ from the plan's `?10`, otherwise \
             `updated_at = ?10` would MATCH and nothing would refuse the in-place arm (and the \
             guarded delete's `updated_at = ?6`, bound to the same value, would MATCH too and \
             fire, leaving `changes() = 1` and selecting the absorbed arm instead)"
    );
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        let affected = writer
            .execute(SqlStatement {
                sql: "UPDATE graph_edges SET updated_at = ?1 WHERE id = ?2".to_string(),
                params: vec![
                    SqlValue::Integer(stored_pinned),
                    SqlValue::Text(requested_id.to_string()),
                ],
                label: Some("test-pin-stored-revision".to_string()),
            })
            .await
            .expect("pin the stored revision");
        assert_eq!(affected, 1, "the pin must touch exactly the requested edge");
    }
    assert!(
        absorb_replacement > stored_pinned,
        "fixture premise: the plan's `?7` must still strictly advance past the stored \
             revision, otherwise `?7 > updated_at` refuses too and the refusal stops being \
             attributable to `updated_at = ?10`"
    );

    // A symmetric plan carries TWO guarded statements, and it matters
    // which one refuses.
    //
    //   1. `edge-symmetric-delete-if-conflict`, guarded
    //      `{expected_min: 0, expected_max: Some(1)}` — a zero-row delete
    //      SATISFIES this guard. It does not roll the unit back. Its
    //      effect here is to leave SQLite's `changes()` at 0.
    //   2. `edge-symmetric-absorb-or-update-inplace`, guarded
    //      `exactly(1)` — this is the statement that refuses and rolls the
    //      unit back, through its in-place arm
    //      `(id = ?2 AND changes() = 0 AND updated_at = ?10 AND
    //        deleted_at IS ?11 AND ?7 > updated_at)`,
    //      whose `updated_at = ?10` is the stale-revision guard under
    //      test. The absorbed arm needs `changes() = 1`, so the delete
    //      refusing is what selects the in-place arm.
    //
    // So the premises come in two layers. The delete's own predicates
    // (`?6`, `?7`, and the EXISTS survivor at `?3`/`?4`/`?5`) are premised
    // because they decide WHETHER the delete fires, and therefore which
    // arm of statement 2 is live. Statement 2's remaining conjuncts are
    // premised because each of them refuses identically to the revision
    // guard this test names. Read the plans' own bound values and check
    // the world against them, immediately before the DML.
    {
        let statements = match &plan {
            AtomicOpPlan::Update(p) => p.statements.clone(),
            other => panic!("expected an Update plan, got {other:?}"),
        };
        // By LABEL, for the same reason as the siblings above.
        let del = statements
            .iter()
            .find(|s| s.statement.label.as_deref() == Some("edge-symmetric-delete-if-conflict"))
            .expect("the plan must carry the guarded symmetric delete");
        let text = |i: usize| match &del.statement.params[i] {
            SqlValue::Text(v) => v.clone(),
            other => panic!("param {i} must be text, got {other:?}"),
        };
        let plan_expected_updated = match &del.statement.params[5] {
            SqlValue::Integer(v) => *v,
            other => panic!("param 5 must be the expected revision, got {other:?}"),
        };
        let plan_expected_deleted = match &del.statement.params[6] {
            SqlValue::Null => None,
            SqlValue::Integer(v) => Some(*v),
            other => panic!("param 6 must be a deletion marker, got {other:?}"),
        };

        // The delete's OWN identity conjuncts. Reading them into the
        // survivor-count panic message below is not asserting them: point
        // `?1` or `?2` at a row that does not exist and the delete still
        // affects zero rows, its `0..=1` guard still accepts that, and
        // statement 2 still refuses on the pinned stale revision — so the
        // test stays green while establishing nothing about which row the
        // named delete attempted.
        assert_eq!(
            text(0),
            "local",
            "fixture premise: the delete's `?1` must be the namespace the row lives in, \
                 otherwise `namespace = ?1` refuses on its own"
        );
        assert_eq!(
            text(1),
            requested_id.to_string(),
            "fixture premise: the delete's `?2` must be the edge under test, otherwise \
                 `id = ?2` refuses on identity and the delete never attempted the requested row"
        );

        let requested_now = runtime
            .get_edge_including_deleted(&token, requested_id)
            .await
            .expect("read the requested edge")
            .expect("the requested edge is present before the plan runs");
        assert_ne!(
            requested_now.updated_at.timestamp_micros(),
            plan_expected_updated,
            "fixture premise: the concurrent writer must actually have moved the \
                 revision past the plan's `?6`, otherwise nothing refuses the delete"
        );
        assert_eq!(
            requested_now.deleted_at.map(|d| d.timestamp_micros()),
            plan_expected_deleted,
            "fixture premise: the stored deletion marker must MATCH the plan's `?7`, \
                 otherwise `deleted_at IS ?7` refuses too and the refusal is not \
                 attributable to the revision guard"
        );

        // The EXISTS arm. A missing survivor refuses the delete on its own
        // and looks identical from outside, so the premise has to be that
        // arm's OWN question. Do not reconstruct it from the seeded row's
        // endpoints: the natural key the plan binds is the canonical
        // ordering, which need not equal the order the survivor was stored
        // in, and a hand-built comparison would be asserting my model of
        // canonicalisation rather than the predicate. Run the subquery
        // verbatim against the plan's own bound parameters instead.
        assert_ne!(
            canonical_id, requested_id,
            "fixture premise: the survivor must be a DIFFERENT row, since the EXISTS \
                 arm excludes the requested id"
        );
        let survivors = {
            let mut reader = runtime.sql().reader().await.expect("sql reader");
            reader
                .query_scalar(SqlStatement {
                    sql: "SELECT count(*) FROM graph_edges \
                              WHERE namespace = ?1 AND source_id = ?3 AND target_id = ?4 \
                                AND relation = ?5 AND id != ?2"
                        .to_string(),
                    params: del.statement.params[0..5].to_vec(),
                    label: Some("test-survivor-exists-premise".to_string()),
                })
                .await
                .expect("run the plan's own EXISTS predicate")
        };
        let survivors = match survivors {
            Some(SqlValue::Integer(n)) => n,
            other => panic!("count(*) must come back as an integer, got {other:?}"),
        };
        assert_eq!(
            survivors,
            1,
            "fixture premise: exactly one survivor must satisfy the plan's own EXISTS \
                 predicate (namespace {}, source {}, target {}, relation {}, id != {}), \
                 otherwise the delete refuses on the survivor arm and the refusal is not \
                 attributable to the revision guard",
            text(0),
            text(2),
            text(3),
            text(4),
            text(1),
        );
    }

    // Statement 2's remaining conjuncts. Each of these refuses the in-place
    // arm identically to the revision guard this test names, so each has to
    // be shown satisfied for the attribution to hold.
    assert_eq!(
        absorb_ns, "local",
        "fixture premise: the plan's `?1` must be the namespace the row lives in, \
             otherwise the outer `namespace = ?1` refuses on its own"
    );
    assert_eq!(
        absorb_id,
        requested_id.to_string(),
        "fixture premise: the plan's `?2` must be the edge under test, otherwise the \
             in-place arm's `id = ?2` refuses on identity"
    );
    {
        let stored = runtime
            .get_edge_including_deleted(&token, requested_id)
            .await
            .expect("read the requested edge")
            .expect("the requested edge is present before the plan runs");
        assert_eq!(
            stored.updated_at.timestamp_micros(),
            stored_pinned,
            "fixture premise: the pin must be what the guard reads at DML time, so the \
                 stored revision is the pinned value and nothing re-advanced it"
        );
        assert_eq!(
            stored.deleted_at.map(|d| d.timestamp_micros()),
            absorb_expected_deleted,
            "fixture premise: the stored deletion marker must MATCH the plan's `?11`, \
                 otherwise `deleted_at IS ?11` refuses too and the refusal stops being \
                 attributable to `updated_at = ?10`"
        );
    }

    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("the seam call itself must not error; the unit rolls back cleanly");
    match outcome {
        crate::atomic_runner::AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            assert_eq!(
                failed_op_index, 0,
                "the sole op's guard must be the one that fails"
            );
            // The decisive attribution, and the reason the two-layer
            // premise stack above exists: `failed_op_index` cannot
            // distinguish the two statements, because both live in op 0.
            // Naming the label is what says the in-place absorb refused
            // rather than the delete — and the delete's `0..=1` guard
            // means it CANNOT be the refuser, so a run that reported it
            // here would be evidence the guards had been rewired.
            assert_eq!(
                failure,
                crate::atomic_runner::AtomicOpFailure::GuardFailed {
                    statement_label: Some("edge-symmetric-absorb-or-update-inplace".to_string()),
                    expected: crate::atomic_plan::AffectedRowGuard::exactly(1),
                    observed: 0,
                },
                "the guarded in-place absorb must be the statement whose guard refused"
            );
        }
        other => panic!(
            "a stale absorption plan must roll back, not silently absorb E into the \
                 survivor and discard the concurrent writer's change: {other:?}"
        ),
    }

    let requested_after = runtime
        .get_edge(&token, requested_id)
        .await
        .expect("get_edge")
        .expect("E must still exist after the rolled-back absorption");
    assert_eq!(
        requested_after.relation,
        EdgeRelation::Extends,
        "E must be untouched by the rolled-back absorption: {requested_after:?}"
    );
    assert!(
        (requested_after.weight - 0.77).abs() < 1e-9,
        "the concurrent writer's committed weight must survive the rolled-back plan: \
             {requested_after:?}"
    );

    let canonical_after = runtime
        .get_edge(&token, canonical_id)
        .await
        .expect("get_edge")
        .expect("S must still exist after the rolled-back absorption");
    assert_eq!(
        canonical_after.weight, 0.6,
        "the survivor must never have been touched by the aborted absorption"
    );
}

/// `update` rejects an entity/note-only field (`name`) on an edge
/// target, mirroring
/// `khive-pack-kg::handlers::update::reject_inapplicable_fields`'s
/// `KindSpec::Edge` arm.
#[tokio::test]
async fn atomic_update_edge_rejects_entity_only_field_name() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "GapEdgeRejectA");
    let b = khive_storage::Entity::new("local", "concept", "GapEdgeRejectB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");
    let edge = runtime
        .link(&token, a_id, b_id, EdgeRelation::Extends, 0.5, None)
        .await
        .expect("seed edge");
    let edge_id = Uuid::from(edge.id);

    let err = prepare_update(
        &runtime,
        &token,
        &json!({"id": edge_id.to_string(), "name": "not-a-valid-edge-field"}),
        None,
    )
    .await
    .expect_err("edge update with an entity-only field must be rejected");
    let message = err.to_string();
    assert!(
        message.contains("name") && message.contains("edge"),
        "error must name the offending field and the substrate: {message}"
    );
}

/// `delete` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`; this
/// asserts `prepare_delete` actually builds a plan for one on both soft
/// and hard delete, matching `operations::delete_edge`'s row-mode DML
/// and unconditional `EdgeDeleted` event.
#[tokio::test]
async fn atomic_delete_edge_soft_and_hard_appends_edge_deleted_event() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    for hard in [false, true] {
        let entities = runtime.entities(&token).expect("entities store");
        let a = khive_storage::Entity::new("local", "concept", format!("GapEdgeDelA{hard}"));
        let b = khive_storage::Entity::new("local", "concept", format!("GapEdgeDelB{hard}"));
        let (a_id, b_id) = (a.id, b.id);
        entities.upsert_entity(a).await.expect("seed a");
        entities.upsert_entity(b).await.expect("seed b");
        let edge = runtime
            .link(&token, a_id, b_id, EdgeRelation::Extends, 0.5, None)
            .await
            .expect("seed edge");
        let edge_id = Uuid::from(edge.id);

        let args = if hard {
            json!({"id": edge_id.to_string(), "hard": true})
        } else {
            json!({"id": edge_id.to_string()})
        };
        let plan = prepare_delete(&runtime, &token, &args, None)
            .await
            .unwrap_or_else(|e| panic!("prepare delete edge (hard={hard}): {e}"));
        let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap_or_else(|e| panic!("edge delete commit (hard={hard}): {e}"));
        assert!(
            matches!(
                outcome,
                crate::atomic_runner::AtomicRunOutcome::Committed { .. }
            ),
            "expected a clean edge delete commit (hard={hard}): {outcome:?}"
        );

        let after = runtime
            .get_edge_including_deleted(&token, edge_id)
            .await
            .expect("get_edge_including_deleted");
        if hard {
            assert!(after.is_none(), "hard delete must purge the row entirely");
        } else {
            assert!(
                after.as_ref().is_some_and(|e| e.deleted_at.is_some()),
                "soft delete must tombstone, not purge"
            );
        }

        let events = events_for_target(&runtime, &token, edge_id, EventKind::EdgeDeleted).await;
        assert_eq!(
            events.len(),
            1,
            "expected exactly one EdgeDeleted event for {edge_id} (hard={hard})"
        );
        assert_eq!(events[0].payload["hard"], json!(hard));
    }
}

/// Parity boundary: an atomic `update` of a note appends exactly one
/// `NoteUpdated` event, because this path and canonical `update_note` build
/// their plan through the same `prepare_versioned_note_update`, which is
/// where the event statements are added.
///
/// This test used to assert the opposite. That was a faithful record of a
/// gap rather than a contract: notes were the substrate that recorded no
/// update at all, so the parity it certified was parity with nothing.
#[tokio::test]
async fn atomic_update_note_appends_its_domain_event() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut note = khive_storage::note::Note::new("local", "observation", "gap1-note-noevent");
    note.name = Some("gap1-note-noevent".to_string());
    let note_id = note.id;
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed note");

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": note_id.to_string(), "content": "gap1-note-noevent, revised"}),
        None,
    )
    .await
    .expect("prepare update");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));

    let event_store = runtime.events(&token).expect("event store");
    let page = event_store
        .query_events(
            khive_storage::EventFilter::default(),
            khive_storage::types::PageRequest::default(),
        )
        .await
        .expect("query_events");
    let for_note: Vec<_> = page
        .items
        .iter()
        .filter(|e| e.target_id == Some(note_id))
        .collect();
    assert_eq!(
        for_note.len(),
        1,
        "an atomic note update must append exactly one event; found: {for_note:?}"
    );
    assert_eq!(for_note[0].kind, EventKind::NoteUpdated);
    assert_eq!(for_note[0].substrate, SubstrateKind::Note);
    assert_eq!(for_note[0].verb, "update");
    assert_eq!(for_note[0].payload["id"], json!(note_id));
    assert_eq!(for_note[0].payload["text_changed"], json!(true));
}

/// Atomic `link` commits its mutation and event-plane observation in the
/// same unit.
#[tokio::test]
async fn atomic_link_appends_created_event_with_edge_observation() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let source = khive_storage::Entity::new("local", "concept", "gap1-link-source");
    let target = khive_storage::Entity::new("local", "concept", "gap1-link-target");
    let (source_id, target_id) = (source.id, target.id);
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(source)
        .await
        .expect("seed source");
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(target)
        .await
        .expect("seed target");

    let plan = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": source_id.to_string(),
            "target_id": target_id.to_string(),
            "relation": "extends",
        }),
    )
    .await
    .expect("prepare link");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("seam call ok");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));

    let event_store = runtime.events(&token).expect("event store");
    let page = event_store
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::LinkCreated],
                ..khive_storage::EventFilter::default()
            },
            khive_storage::types::PageRequest::default(),
        )
        .await
        .expect("query_events");
    assert_eq!(page.items.len(), 1, "link must append one created event");
    let event = &page.items[0];
    assert_eq!(event.payload["mutation"], "created");
    assert_eq!(event.payload["source_kind"], "entity");
    assert_eq!(event.payload["target_kind"], "entity");
    let edge_id = event.target_id.expect("link event targets its edge");
    let observed = event_store
        .query_events(
            khive_storage::EventFilter {
                observed: vec![edge_id],
                ..khive_storage::EventFilter::default()
            },
            khive_storage::types::PageRequest::default(),
        )
        .await
        .expect("query observed edge");
    assert_eq!(observed.items.len(), 1);
    assert_eq!(observed.items[0].id, event.id);
}

#[tokio::test]
async fn atomic_note_link_projects_note_endpoints() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let source = khive_storage::note::Note::new("local", "observation", "source");
    let target = khive_storage::note::Note::new("local", "insight", "target");
    let (source_id, target_id) = (source.id, target.id);
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(source)
        .await
        .expect("seed source");
    runtime
        .notes(&token)
        .expect("notes store")
        .upsert_note(target)
        .await
        .expect("seed target");

    let plan = prepare_link(
        &runtime,
        &token,
        &json!({
            "source_id": source_id.to_string(),
            "target_id": target_id.to_string(),
            "relation": "supports",
        }),
    )
    .await
    .expect("prepare note link");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit note link");
    assert!(matches!(
        outcome,
        crate::atomic_runner::AtomicRunOutcome::Committed { .. }
    ));

    let page = runtime
        .events(&token)
        .expect("event store")
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![EventKind::LinkCreated],
                ..khive_storage::EventFilter::default()
            },
            khive_storage::types::PageRequest::default(),
        )
        .await
        .expect("query link event");
    assert_eq!(page.items.len(), 1);
    let event = &page.items[0];
    assert_eq!(event.payload["source_kind"], "note");
    assert_eq!(event.payload["target_kind"], "note");

    let query = format!(
        "MATCH (ev)-[:observed_as_target]->(t) WHERE ev.id = '{}' RETURN t.id",
        event.id
    );
    let rows = runtime
        .query(&token, &query)
        .await
        .expect("query observations");
    assert_eq!(rows.len(), 2);
    let observed_ids: std::collections::BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row.columns.iter())
        .filter_map(|column| match &column.value {
            khive_storage::types::SqlValue::Text(value) => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        observed_ids,
        std::collections::BTreeSet::from([source_id.to_string(), target_id.to_string()])
    );
}

// ------------------------------------------------------------------
// AddEntity and AddNote plans alongside link
// ------------------------------------------------------------------

#[tokio::test]
async fn prepare_add_entity_rejects_whitespace_only_name() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let err = prepare_add_entity(&runtime, &token, &json!({"kind": "concept", "name": "   "}))
        .await
        .expect_err("whitespace-only entity name must fail prepare");

    assert!(matches!(
        err,
        RuntimeError::InvalidInput(message) if message.contains("name must not be empty")
    ));
}

#[tokio::test]
async fn prepare_add_entity_rejects_non_string_description() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let err = prepare_add_entity(
        &runtime,
        &token,
        &json!({"kind": "concept", "name": "Valid", "description": 42}),
    )
    .await
    .expect_err("non-string entity description must fail prepare");

    assert!(matches!(
        err,
        RuntimeError::InvalidInput(message)
            if message.contains("description must be a string or null")
    ));
}

#[tokio::test]
async fn prepare_add_note_rejects_non_string_name() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let err = prepare_add_note(
        &runtime,
        &token,
        &json!({"kind": "observation", "content": "Valid", "name": 42}),
    )
    .await
    .expect_err("non-string note name must fail prepare");

    assert!(matches!(
        err,
        RuntimeError::InvalidInput(message) if message.contains("name must be a string or null")
    ));
}

/// ADR-115 Amendment 1 §3: proposal materialization (`prepare_add_entity`)
/// must reject the reserved `khive:secret_gate` property key the same way
/// canonical `create` does — proposal apply is reservation-only, never an
/// exemption-consuming path.
#[tokio::test]
async fn prepare_add_entity_rejects_reserved_secret_gate_property() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let err = prepare_add_entity(
        &runtime,
        &token,
        &json!({
            "kind": "concept",
            "name": "ReservedKeyEntity",
            "properties": {"khive:secret_gate": "exempted:content-sha256-manifest-v1"},
        }),
    )
    .await
    .expect_err("a caller-supplied reserved key on a new entity must be rejected");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate") && msg.contains("runtime-owned")),
        "expected a reservation rejection, got: {err:?}"
    );
}

/// Note-substrate counterpart of
/// `prepare_add_entity_rejects_reserved_secret_gate_property`.
#[tokio::test]
async fn prepare_add_note_rejects_reserved_secret_gate_property() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    let err = prepare_add_note(
        &runtime,
        &token,
        &json!({
            "kind": "observation",
            "content": "a note carrying a reserved property key",
            "properties": {"khive:secret_gate": "exempted:content-sha256-manifest-v1"},
        }),
    )
    .await
    .expect_err("a caller-supplied reserved key on a new note must be rejected");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate") && msg.contains("runtime-owned")),
        "expected a reservation rejection, got: {err:?}"
    );
}

#[tokio::test]
async fn atomic_proposal_vectors_materialize_only_after_successful_commit() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let vec_store = runtime
        .vectors_for_model(&token, STUB_MODEL)
        .expect("vec store");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "ProposalPlanLinkA");
    let b = khive_storage::Entity::new("local", "concept", "ProposalPlanLinkB");
    let (a_id, b_id) = (a.id, b.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(b).await.expect("seed b");

    let add_entity_plan = prepare_add_entity(
        &runtime,
        &token,
        &json!({"kind": "concept", "name": "ProposalPlanNewEntity", "description": "created atomically"}),
    )
    .await
    .expect("prepare add_entity");
    let link_plan = prepare_link(
        &runtime,
        &token,
        &json!({"source_id": a_id.to_string(), "target_id": b_id.to_string(), "relation": "extends"}),
    )
    .await
    .expect("prepare link");
    let add_note_plan = prepare_add_note(
        &runtime,
        &token,
        &json!({"kind": "observation", "content": "created atomically alongside the entity"}),
    )
    .await
    .expect("prepare add_note");

    let entity_id = match &add_entity_plan {
        AtomicOpPlan::AddEntity(p) => p.entity_id,
        other => panic!("expected an AddEntity plan, got {other:?}"),
    };
    let note_id = match &add_note_plan {
        AtomicOpPlan::AddNote(p) => p.note_id,
        other => panic!("expected an AddNote plan, got {other:?}"),
    };

    let outcome = crate::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![add_entity_plan, link_plan, add_note_plan],
    )
    .await
    .expect("seam call ok");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected the whole unit to commit: {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[
            PostCommitEffect::ReindexEntity { entity_id },
            PostCommitEffect::ReindexNote {
                note_id,
                version: 1
            },
        ],
        "prepare-derived effects must reach the committed token unchanged"
    );
    assert_eq!(
        vec_store.count().await.expect("count before effects"),
        0,
        "commit returns deferred effects without materializing vectors"
    );
    let entity = runtime
        .entities(&token)
        .expect("entities store")
        .get_entity(entity_id)
        .await
        .expect("get_entity")
        .expect("entity must exist after commit");
    assert_eq!(entity.name, "ProposalPlanNewEntity");
    assert!(
        runtime
            .text(&token)
            .expect("text store")
            .get_document("local", entity_id)
            .await
            .expect("get_document")
            .is_some(),
        "entity's FTS document must exist after commit"
    );

    let (edge_count, _, _, edge_deleted_at) =
        probe_edge_natural_key(&runtime, "local", a_id, b_id, "extends").await;
    assert_eq!(
        edge_count, 1,
        "the edge must be committed alongside the entity/note"
    );
    assert!(edge_deleted_at.is_none());

    let note = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(note_id)
        .await
        .expect("get_note")
        .expect("note must exist after commit");
    assert_eq!(note.content, "created atomically alongside the entity");
    assert!(
        runtime
            .text_for_notes(&token)
            .expect("text store")
            .get_document("local", note_id)
            .await
            .expect("get_document")
            .is_some(),
        "note's FTS document must exist after commit"
    );

    apply_post_commit_effects(&runtime, &token, post_commit)
        .await
        .expect("apply post-commit effects");

    assert_eq!(
        vec_store.count().await.expect("count after"),
        2,
        "post-commit reindex must have embedded both the new entity and the new note"
    );
}

#[tokio::test]
async fn atomic_proposal_abort_leaves_zero_vector_rows() {
    let runtime = scratch_runtime();
    runtime.register_embedder(StubProvider);
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let vec_store = runtime
        .vectors_for_model(&token, STUB_MODEL)
        .expect("vec store");
    let entities = runtime.entities(&token).expect("entities store");
    let a = khive_storage::Entity::new("local", "concept", "ProposalPlanRollbackA");
    let x = khive_storage::Entity::new("local", "concept", "ProposalPlanRollbackX");
    let (a_id, x_id) = (a.id, x.id);
    entities.upsert_entity(a).await.expect("seed a");
    entities.upsert_entity(x.clone()).await.expect("seed x");

    let add_entity_plan = prepare_add_entity(
        &runtime,
        &token,
        &json!({"kind": "concept", "name": "ProposalPlanRollbackNewEntity"}),
    )
    .await
    .expect("prepare add_entity");
    let add_note_plan = prepare_add_note(
        &runtime,
        &token,
        &json!({"kind": "observation", "content": "must not survive the rollback"}),
    )
    .await
    .expect("prepare add_note");
    let delete_plan = prepare_delete(
        &runtime,
        &token,
        &json!({"id": x_id.to_string(), "hard": true}),
        None,
    )
    .await
    .expect("prepare delete x");
    // Prepare sees x before the transaction; the guarded link must detect
    // that the preceding hard delete removed it inside the transaction.
    let link_plan = prepare_link(
        &runtime,
        &token,
        &json!({"source_id": a_id.to_string(), "target_id": x_id.to_string(), "relation": "extends"}),
    )
    .await
    .expect("prepare link (endpoint still exists at prepare time)");

    let entity_id = match &add_entity_plan {
        AtomicOpPlan::AddEntity(p) => p.entity_id,
        other => panic!("expected an AddEntity plan, got {other:?}"),
    };
    let note_id = match &add_note_plan {
        AtomicOpPlan::AddNote(p) => p.note_id,
        other => panic!("expected an AddNote plan, got {other:?}"),
    };

    let outcome = crate::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![add_entity_plan, add_note_plan, delete_plan, link_plan],
    )
    .await
    .expect("the seam call itself must not error; the unit rolls back cleanly");
    match outcome {
        crate::atomic_runner::AtomicRunOutcome::RolledBack {
            failed_op_index, ..
        } => {
            assert_eq!(
                failed_op_index, 3,
                "the trailing link (index 3) must be the op whose guard fails"
            );
        }
        other => panic!("expected the whole unit to roll back, got {other:?}"),
    }

    assert_eq!(
        vec_store
            .count()
            .await
            .expect("vector count after rollback"),
        0,
        "a rolled-back atomic apply must not materialize vectors"
    );

    assert!(
        runtime
            .get_entity_including_deleted(&token, entity_id)
            .await
            .expect("get_entity_including_deleted")
            .is_none(),
        "the new entity must leave zero trace after rollback"
    );
    assert!(
        runtime
            .text(&token)
            .expect("text store")
            .get_document("local", entity_id)
            .await
            .expect("get_document")
            .is_none(),
        "the new entity's FTS document must leave zero trace after rollback"
    );
    assert!(
        runtime
            .get_note_including_deleted(&token, note_id)
            .await
            .expect("get_note_including_deleted")
            .is_none(),
        "the new note must leave zero trace after rollback"
    );
    assert!(
        runtime
            .text_for_notes(&token)
            .expect("text store")
            .get_document("local", note_id)
            .await
            .expect("get_document")
            .is_none(),
        "the new note's FTS document must leave zero trace after rollback"
    );

    let x_after = runtime
        .get_entity_including_deleted(&token, x_id)
        .await
        .expect("get_entity_including_deleted")
        .expect("x must still be present because its delete rolled back too");
    assert!(
        x_after.deleted_at.is_none(),
        "x's delete must have rolled back along with the failed link"
    );

    let (edge_count, _, _, _) =
        probe_edge_natural_key(&runtime, "local", a_id, x_id, "extends").await;
    assert_eq!(edge_count, 0, "no edge may have been committed");
}

/// ADR-014 tri-state on the atomic path: `entity_type: null` must
/// explicitly CLEAR a stored entity type (and reindex), not collapse to
/// "unchanged" like the old `optional_create_string` did.
#[tokio::test]
async fn atomic_update_entity_type_null_clears_stored_type() {
    let runtime = scratch_runtime();
    runtime.install_entity_type_validator(std::sync::Arc::new(|kind, entity_type| {
        let Some(raw) = entity_type else {
            return Ok(None);
        };
        let normalized = raw.trim().to_ascii_lowercase();
        if kind == "concept" && normalized == "algorithm" {
            Ok(Some(normalized))
        } else {
            Err(RuntimeError::InvalidInput(format!(
                "unknown entity_type {raw:?} for {kind:?}; valid: algorithm"
            )))
        }
    }));
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let mut entity = khive_storage::Entity::new("local", "concept", "AtomicNullClear");
    entity.entity_type = Some("algorithm".to_string());
    let entity_id = entity.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(entity)
        .await
        .expect("seed entity");

    let plan = prepare_update(
        &runtime,
        &token,
        &json!({"id": entity_id.to_string(), "entity_type": null}),
        None,
    )
    .await
    .expect("atomic prepare must accept entity_type: null");
    let outcome = crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("atomic update must run");
    let post_commit = match outcome {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[PostCommitEffect::ReindexEntity { entity_id }],
        "a type clear that differs from the prior value must reindex"
    );

    let updated = runtime
        .get_entity(&token, entity_id)
        .await
        .expect("read updated entity");
    assert_eq!(
        updated.entity_type, None,
        "entity_type: null must clear the stored type"
    );
    assert_eq!(updated.name, "AtomicNullClear");
}

/// ADR-014 tri-state on the atomic path: a PRESENT `entity_type` key,
/// including JSON `null`, must be rejected on note and edge targets
/// (parity with `khive-pack-kg`'s `reject_inapplicable_fields`).
#[tokio::test]
async fn atomic_update_null_entity_type_rejected_for_note_and_edge() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let note = runtime
        .create_note(
            &token,
            "observation",
            None,
            "note body for entity_type guard",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("create note");

    let note_err = prepare_update(
        &runtime,
        &token,
        &json!({"id": note.id.to_string(), "entity_type": null}),
        Some(crate::atomic_prepare::AtomicUpdateKind::Note { specific: None }),
    )
    .await
    .expect_err("entity_type: null on a note must be rejected");
    assert!(
        matches!(note_err, RuntimeError::InvalidInput(ref msg) if msg.contains("entity_type") && msg.contains("not valid for a note")),
        "expected an InvalidInput naming entity_type for a note, got: {note_err:?}"
    );

    let source = khive_storage::Entity::new("local", "concept", "AtomicNullTypeEdgeSource");
    let target = khive_storage::Entity::new("local", "concept", "AtomicNullTypeEdgeTarget");
    let source_id = source.id;
    let target_id = target.id;
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(source)
        .await
        .expect("seed source");
    runtime
        .entities(&token)
        .expect("entities store")
        .upsert_entity(target)
        .await
        .expect("seed target");
    let edge = runtime
        .link(
            &token,
            source_id,
            target_id,
            "supports".parse().expect("relation"),
            0.5,
            None,
        )
        .await
        .expect("create edge");

    let edge_err = prepare_update(
        &runtime,
        &token,
        &json!({"id": edge.id.to_string(), "entity_type": null}),
        Some(crate::atomic_prepare::AtomicUpdateKind::Edge),
    )
    .await
    .expect_err("entity_type: null on an edge must be rejected");
    assert!(
        matches!(edge_err, RuntimeError::InvalidInput(ref msg) if msg.contains("entity_type") && msg.contains("not valid for an edge")),
        "expected an InvalidInput naming entity_type for an edge, got: {edge_err:?}"
    );
}
