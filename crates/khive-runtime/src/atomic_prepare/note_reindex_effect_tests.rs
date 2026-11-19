use std::sync::{Arc, Mutex};

use khive_types::Namespace;
use serde_json::json;

use super::*;
use crate::atomic_prepare::{
    apply_post_commit_effects_with_report, prepare_delete, prepare_update,
};
use crate::atomic_runner::{run_atomic_unit, AtomicRunOutcome};
use crate::runtime::RuntimeConfig;

fn scratch_runtime(dir: &tempfile::TempDir) -> KhiveRuntime {
    KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(dir.path().join("note_reindex_effect.db")),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime")
}

fn local_token(runtime: &KhiveRuntime) -> NamespaceToken {
    runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize")
}

/// Install a hook that records the id of every note it is told about.
fn count_hook(runtime: &KhiveRuntime) -> Arc<Mutex<Vec<Uuid>>> {
    let fired = Arc::new(Mutex::new(Vec::new()));
    let hook_fired = fired.clone();
    runtime.install_note_mutation_hook(Arc::new(move |_kind: String, id: Uuid| {
        let fired = hook_fired.clone();
        Box::pin(async move { fired.lock().expect("lock").push(id) })
    }));
    fired
}

async fn seed_note(runtime: &KhiveRuntime, token: &NamespaceToken) -> Note {
    let note = Note::new("local", "observation", "reindex effect target");
    let id = note.id;
    let notes = runtime.notes(token).expect("notes store");
    notes.upsert_note(note).await.expect("seed note");
    notes
        .get_note(id)
        .await
        .expect("read seeded note")
        .expect("seeded note exists")
}

/// Make the full-text stage of every later note reindex fail.
async fn drop_note_text_index(runtime: &KhiveRuntime) {
    let mut writer = runtime.sql().writer().await.expect("writer");
    writer
        .execute(khive_storage::SqlStatement {
            sql: "DROP TABLE fts_notes".into(),
            params: Vec::new(),
            label: Some("test_remove_fts_notes_after_commit".into()),
        })
        .await
        .expect("remove FTS table");
}

/// Another writer commits a newer version of `note`.
async fn advance_note(runtime: &KhiveRuntime, token: &NamespaceToken, note: &Note) {
    let mut newer = note.clone();
    newer.content = "newer version from another writer".into();
    newer.updated_at += 1;
    runtime
        .notes(token)
        .expect("notes store")
        .upsert_note(newer)
        .await
        .expect("advance note");
    let current = runtime
        .notes(token)
        .expect("notes store")
        .get_note(note.id)
        .await
        .expect("read advanced note")
        .expect("advanced note exists");
    assert_eq!(current.version, note.version + 1);
}

#[tokio::test]
async fn reindex_effect_failed_text_reindex_notifies_hook_once_and_returns_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = scratch_runtime(&dir);
    let token = local_token(&runtime);
    let fired = count_hook(&runtime);
    let note = seed_note(&runtime, &token).await;
    drop_note_text_index(&runtime).await;
    let expected = runtime
        .reindex_note(&token, &note)
        .await
        .expect_err("the full-text stage must fail without its index")
        .to_string();

    let error = apply(&runtime, &token, note.id, note.version)
        .await
        .expect_err("a failed reindex must still be returned")
        .to_string();

    assert_eq!(error, expected);
    assert_eq!(*fired.lock().expect("lock"), vec![note.id]);
}

#[tokio::test]
async fn reindex_effect_failed_reindex_with_moved_version_returns_error_without_hook() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = scratch_runtime(&dir);
    let token = local_token(&runtime);
    let fired = count_hook(&runtime);
    let note = seed_note(&runtime, &token).await;
    drop_note_text_index(&runtime).await;
    let reindex = runtime.reindex_note(&token, &note).await;
    let expected = reindex
        .as_ref()
        .expect_err("the full-text stage must fail without its index")
        .to_string();
    advance_note(&runtime, &token, &note).await;

    let error = notify_if_current(&runtime, &token, &note, reindex)
        .await
        .expect_err("the original reindex error must still be returned")
        .to_string();

    assert_eq!(error, expected);
    assert!(fired.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn reindex_effect_successful_reindex_notifies_hook_exactly_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = scratch_runtime(&dir);
    let token = local_token(&runtime);
    let fired = count_hook(&runtime);
    let note = seed_note(&runtime, &token).await;

    let outcome = apply(&runtime, &token, note.id, note.version)
        .await
        .expect("a healthy reindex succeeds")
        .expect("a current note yields an outcome");

    assert_eq!(
        outcome.effect,
        PostCommitEffect::ReindexNote {
            note_id: note.id,
            version: note.version,
        }
    );
    assert_eq!(*fired.lock().expect("lock"), vec![note.id]);
}

#[tokio::test]
async fn reindex_effect_moved_version_after_successful_reindex_has_no_hook_or_outcome() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = scratch_runtime(&dir);
    let token = local_token(&runtime);
    let fired = count_hook(&runtime);
    let note = seed_note(&runtime, &token).await;
    advance_note(&runtime, &token, &note).await;
    let reindex = Ok(EmbeddingTruncationReport::default());

    let outcome = notify_if_current(&runtime, &token, &note, reindex)
        .await
        .expect("a moved version is not a failure");

    assert!(outcome.is_none());
    assert!(fired.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn reindex_effect_later_effect_still_runs_after_failed_reindex() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = scratch_runtime(&dir);
    let token = local_token(&runtime);
    let fired = count_hook(&runtime);
    let reindexed = seed_note(&runtime, &token).await;
    let removed = seed_note(&runtime, &token).await;
    let update = prepare_update(
        &runtime,
        &token,
        // Explicit embedding keeps the reindex effect: without it an update
        // that finds no existing vector row schedules a plain notification.
        &json!({"id": reindexed.id.to_string(), "content": "revised", "embed": true}),
        None,
    )
    .await
    .expect("prepare note update");
    let delete = prepare_delete(
        &runtime,
        &token,
        &json!({"id": removed.id.to_string(), "hard": false}),
        None,
    )
    .await
    .expect("prepare note delete");
    let plans = vec![update, delete];
    let outcome = run_atomic_unit(runtime.sql().as_ref(), plans)
        .await
        .expect("commit atomic unit");
    let post_commit = match outcome {
        AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(
        post_commit.as_slice(),
        &[
            PostCommitEffect::ReindexNote {
                note_id: reindexed.id,
                version: reindexed.version + 1,
            },
            PostCommitEffect::NoteDeleted {
                note_id: removed.id,
                kind: "observation".into(),
            },
        ],
        "the fixture must commit the reindex before the deletion"
    );
    drop_note_text_index(&runtime).await;

    let error = apply_post_commit_effects_with_report(&runtime, &token, post_commit)
        .await
        .expect_err("the reindex effect must fail")
        .to_string();

    assert!(error.contains("effect[0]"), "{error}");
    assert!(!error.contains("effect[1]"), "{error}");
    let expected_hooks = vec![reindexed.id, removed.id];
    assert_eq!(*fired.lock().expect("lock"), expected_hooks);
}
