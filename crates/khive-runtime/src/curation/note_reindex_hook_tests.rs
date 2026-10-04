use super::*;
use crate::curation::NotePatch;
use crate::note_write::NoteWriteOptions;

fn hook(runtime: &KhiveRuntime) -> Arc<Mutex<Vec<Uuid>>> {
    let fired = Arc::new(Mutex::new(Vec::new()));
    let seen = fired.clone();
    runtime.install_note_mutation_hook(Arc::new(move |_kind, id| {
        let seen = seen.clone();
        Box::pin(async move { seen.lock().unwrap().push(id) })
    }));
    fired
}

async fn commit_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> crate::atomic_runner::CommittedPostCommitEffects {
    let plan = crate::atomic_prepare::prepare_update(
        runtime,
        token,
        &serde_json::json!({"id": id.to_string(), "content": "committed update", "embed": true}),
        None,
    )
    .await
    .unwrap();
    match crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .unwrap()
    {
        crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected committed update, got {other:?}"),
    }
}

#[tokio::test]
async fn committed_update_notifies_once_after_partial_and_all_model_errors() {
    for partial in [false, true] {
        for atomic in [false, true] {
            let runtime = KhiveRuntime::memory().unwrap();
            let token = NamespaceToken::local();
            let note = seed(&runtime, &token, "before error").await;
            let order = if partial {
                register_pair(&runtime, Behavior::BuildFailure)
            } else {
                let order = Arc::new(Mutex::new(Vec::new()));
                runtime.register_embedder(Provider {
                    name: "broken".into(),
                    behavior: Behavior::BuildFailure,
                    build_order: order.clone(),
                });
                order
            };
            let fired = hook(&runtime);
            let result = if atomic {
                let effects = commit_update(&runtime, &token, note.id).await;
                crate::atomic_prepare::apply_post_commit_effects_with_report(
                    &runtime, &token, effects,
                )
                .await
                .map(|_| ())
            } else {
                runtime
                    .update_note_with_embedding_report(
                        &token,
                        note.id,
                        NotePatch {
                            content: Some("committed update".into()),
                            write_options: NoteWriteOptions {
                                embed: Some(true),
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|_| ())
            };
            let error = result
                .expect_err("index failure still reaches the caller")
                .to_string();
            assert!(error.contains("model broken embedding"), "{error}");
            assert!(error.contains(&note.id.to_string()), "{error}");
            assert_eq!(*fired.lock().unwrap(), vec![note.id]);
            let current = runtime
                .notes(&token)
                .unwrap()
                .get_note(note.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.content, "committed update");
            assert_eq!(current.version, note.version + 1);
            let document = runtime
                .text_for_notes(&token)
                .unwrap()
                .get_document("local", note.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(document.body, current.content);
            let mut built = order.lock().unwrap().clone();
            built.sort();
            assert_eq!(
                built,
                if partial {
                    vec!["broken".to_string(), "healthy".to_string()]
                } else {
                    vec!["broken".to_string()]
                }
            );
            assert_eq!(
                runtime
                    .vectors_for_model(&token, "broken")
                    .unwrap()
                    .count()
                    .await
                    .unwrap(),
                0
            );
            if partial {
                assert_eq!(
                    runtime
                        .vectors_for_model(&token, "healthy")
                        .unwrap()
                        .count()
                        .await
                        .unwrap(),
                    1
                );
            }
        }
    }
}

struct MovingProvider {
    notes: Arc<dyn khive_storage::NoteStore>,
    id: Uuid,
    builds: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl EmbedderProvider for MovingProvider {
    fn name(&self) -> &str {
        "moving"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let mut current = self.notes.get_note(self.id).await.unwrap().unwrap();
        current.version += 1;
        current.updated_at += 1;
        current.content = "newer version from another writer".into();
        self.notes.upsert_note(current).await.unwrap();
        Err(RuntimeError::Internal(
            "failure after another writer advanced the note".into(),
        ))
    }
}

#[tokio::test]
async fn moved_version_before_or_during_failed_reindex_has_no_hook_or_outcome() {
    for before in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let note = seed(&runtime, &token, "before concurrent update").await;
        let builds = Arc::new(AtomicUsize::new(0));
        let notes = runtime.notes(&token).unwrap();
        runtime.register_embedder(MovingProvider {
            notes: notes.clone(),
            id: note.id,
            builds: builds.clone(),
        });
        let fired = hook(&runtime);
        let effects = commit_update(&runtime, &token, note.id).await;
        assert_eq!(
            effects.as_slice(),
            &[crate::atomic_plan::PostCommitEffect::ReindexNote {
                note_id: note.id,
                version: note.version + 1,
            }]
        );
        if before {
            let mut current = notes.get_note(note.id).await.unwrap().unwrap();
            current.version += 1;
            current.updated_at += 1;
            current.content = "newer version from another writer".into();
            notes.upsert_note(current).await.unwrap();
        }
        let outcomes =
            crate::atomic_prepare::apply_post_commit_effects_with_report(&runtime, &token, effects)
                .await
                .expect("a moved version stays an omitted guarded effect");
        assert!(outcomes.is_empty());
        assert!(fired.lock().unwrap().is_empty());
        assert_eq!(builds.load(Ordering::SeqCst), usize::from(!before));
        let current = notes.get_note(note.id).await.unwrap().unwrap();
        assert_eq!(current.version, note.version + 2);
        assert_eq!(current.content, "newer version from another writer");
    }
}
