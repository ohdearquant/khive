use super::*;
use crate::curation::{ContentMergeStrategy, EntityDedupMergePolicy};
use crate::embedder_registry::EmbedderProvider;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

#[derive(Clone, Copy)]
enum Behavior {
    BuildFailure,
    NonFinite,
    Healthy,
}

struct Service(Behavior);

#[async_trait::async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let value = match self.0 {
            Behavior::NonFinite => f32::NAN,
            _ => 1.0,
        };
        Ok(texts.iter().map(|_| vec![value; 4]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "note-reindex-failure-fixture"
    }
}

struct Provider {
    name: String,
    behavior: Behavior,
    build_order: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.build_order.lock().unwrap().push(self.name.clone());
        if matches!(self.behavior, Behavior::BuildFailure) {
            return Err(RuntimeError::Internal("injected provider failure".into()));
        }
        Ok(Arc::new(Service(self.behavior)))
    }
}

fn register_pair(runtime: &KhiveRuntime, behavior: Behavior) -> Arc<Mutex<Vec<String>>> {
    let order = Arc::new(Mutex::new(Vec::new()));
    for (name, behavior) in [("broken", behavior), ("healthy", Behavior::Healthy)] {
        runtime.register_embedder(Provider {
            name: name.into(),
            behavior,
            build_order: order.clone(),
        });
    }
    order
}

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, content: &str) -> Note {
    runtime
        .create_note(token, "observation", None, content, None, None, vec![])
        .await
        .unwrap()
}

async fn assert_live_text(runtime: &KhiveRuntime, token: &NamespaceToken, note: &Note) {
    let current = runtime
        .notes(token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.content, note.content);
    assert!(current.deleted_at.is_none());
    let document = runtime
        .text_for_notes(token)
        .unwrap()
        .get_document("local", note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(document.body, note.content);
    assert_eq!(
        runtime
            .vectors_for_model(token, "healthy")
            .unwrap()
            .count()
            .await
            .unwrap(),
        1
    );
}

async fn eligible_failure(behavior: Behavior, stage: &str) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let note = seed(&runtime, &token, "caller-visible indexing failure").await;
    let order = register_pair(&runtime, behavior);
    let mut plan = EmbeddingModelPlan::capture(&runtime);
    // Sort the actual captured model set; registry order is unspecified.
    plan.model_names.sort();
    assert_eq!(
        plan.model_names,
        vec!["broken".to_string(), "healthy".to_string()]
    );
    let error = runtime
        .reindex_note_with_plan(&token, &note, &plan)
        .await
        .expect_err("eligible model failure must reach the caller")
        .to_string();
    assert!(error.contains(&note.id.to_string()), "{error}");
    assert!(error.contains(&format!("model broken {stage}")), "{error}");
    assert!(error.contains("truncated=0 discarded_bytes=0"), "{error}");
    assert_eq!(
        *order.lock().unwrap(),
        vec!["broken".to_string(), "healthy".to_string()]
    );
    assert_live_text(&runtime, &token, &note).await;
    assert_eq!(
        runtime
            .vectors_for_model(&token, "broken")
            .unwrap()
            .count()
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn provider_failure_is_reported_and_later_healthy_model_indexes() {
    eligible_failure(Behavior::BuildFailure, "embedding").await;
}

#[tokio::test]
async fn nonfinite_failure_is_reported_and_later_healthy_model_indexes() {
    eligible_failure(Behavior::NonFinite, "vector validation").await;
}

async fn embedding_blob(runtime: &KhiveRuntime, id: Uuid) -> Option<Vec<u8>> {
    runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT embedding FROM vec_broken WHERE subject_id=?1".into(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("test-note-reindex-prior-vector".into()),
        })
        .await
        .unwrap()
        .map(|value| match value {
            SqlValue::Blob(blob) => blob,
            value => panic!("expected vector BLOB, got {value:?}"),
        })
}

async fn sql_failure(publication: bool) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let note = seed(
        &runtime,
        &token,
        "SQL indexing failure keeps text committed",
    )
    .await;
    register_pair(&runtime, Behavior::Healthy);
    runtime.vectors_for_model(&token, "healthy").unwrap();
    let before = if publication {
        runtime
            .vectors_for_model(&token, "broken")
            .unwrap()
            .insert_batch(vec![khive_storage::VectorRecord {
                subject_id: note.id,
                kind: khive_types::SubstrateKind::Note,
                namespace: note.namespace.clone(),
                field: "note.content".into(),
                embedding_model: Some("broken".into()),
                vectors: vec![vec![0.25; 4]],
                text_fingerprint: None,
                updated_at: chrono::Utc::now(),
            }])
            .await
            .unwrap();
        let blob = embedding_blob(&runtime, note.id).await;
        assert!(blob.is_some());
        blob
    } else {
        None
    };
    let attempts = Arc::new(AtomicUsize::new(0));
    {
        // In-memory SQL shares this writer; release its guard before await.
        let writer = runtime.backend().pool().writer().unwrap();
        let attempts = attempts.clone();
        writer.conn().authorizer(Some(move |context: AuthContext<'_>| {
            let deny = if publication {
                matches!(context.action, AuthAction::Insert { table_name } if table_name == "vec_broken")
            } else {
                matches!(context.action, AuthAction::CreateVtable { table_name, .. } if table_name == "vec_broken")
            };
            if deny {
                attempts.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        })).unwrap();
    }
    let mut plan = EmbeddingModelPlan::capture(&runtime);
    plan.model_names.sort();
    let error = runtime
        .reindex_note_with_plan(&token, &note, &plan)
        .await
        .expect_err("real SQL model failure must be disclosed")
        .to_string();
    {
        let writer = runtime.backend().pool().writer().unwrap();
        writer
            .conn()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
    }
    assert!(
        attempts.load(Ordering::SeqCst) > 0,
        "fault must reach the selected SQL seam"
    );
    let stage = if publication {
        "vector publication"
    } else {
        "vector store"
    };
    assert!(error.contains(&format!("model broken {stage}")), "{error}");
    assert_live_text(&runtime, &token, &note).await;
    if publication {
        assert_eq!(
            embedding_blob(&runtime, note.id).await,
            before,
            "failed publication must not erase the prior vector"
        );
    } else {
        assert_eq!(
            runtime
                .vectors_for_model(&token, "broken")
                .unwrap()
                .count()
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn vector_store_failure_is_reported_and_healthy_model_indexes() {
    sql_failure(false).await;
}

#[tokio::test]
async fn vector_publication_failure_is_reported_and_prior_vector_survives() {
    sql_failure(true).await;
}

#[tokio::test]
async fn restore_discloses_embedding_failure_after_live_row_and_text_commit() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let note = seed(
        &runtime,
        &token,
        "restore retains committed searchable text",
    )
    .await;
    assert!(runtime.delete_note(&token, note.id, false).await.unwrap());
    register_pair(&runtime, Behavior::BuildFailure);
    let error = runtime
        .restore_note(&token, note.id)
        .await
        .expect_err("restore must disclose its partial embedding failure")
        .to_string();
    assert!(error.contains("is restored and text-indexed"), "{error}");
    assert!(error.contains("model broken embedding"), "{error}");
    assert_live_text(&runtime, &token, &note).await;
}

#[tokio::test]
async fn atomic_update_discloses_model_failure_without_skipping_later_effect() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let note = seed(&runtime, &token, "before atomic update").await;
    let removed = seed(&runtime, &token, "later independent deletion").await;
    register_pair(&runtime, Behavior::BuildFailure);
    let fired = Arc::new(Mutex::new(Vec::new()));
    let seen = fired.clone();
    runtime.install_note_mutation_hook(Arc::new(move |_kind, id| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(id);
        })
    }));
    let update = crate::atomic_prepare::prepare_update(&runtime, &token, &serde_json::json!({"id": note.id.to_string(), "content": "after atomic update", "embed": true}), None).await.unwrap();
    let delete = crate::atomic_prepare::prepare_delete(
        &runtime,
        &token,
        &serde_json::json!({"id": removed.id.to_string(), "hard": false}),
        None,
    )
    .await
    .unwrap();
    let post_commit =
        match crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![update, delete])
            .await
            .unwrap()
        {
            crate::atomic_runner::AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected committed row unit, got {other:?}"),
        };
    let error =
        crate::atomic_prepare::apply_post_commit_effects_with_report(&runtime, &token, post_commit)
            .await
            .expect_err("after-commit eligible failure must be aggregated")
            .to_string();
    assert!(error.contains("effect[0]"), "{error}");
    assert!(error.contains("model broken embedding"), "{error}");
    assert!(error.contains(&note.id.to_string()), "{error}");
    let updated = runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.content, "after atomic update");
    assert_live_text(&runtime, &token, &updated).await;
    assert!(runtime
        .get_note_including_deleted(&token, removed.id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert_eq!(
        *fired.lock().unwrap(),
        vec![note.id, removed.id],
        "current committed update and later deletion each notify despite the indexing error"
    );
}

#[tokio::test]
async fn merge_preserves_observed_truncation_and_discloses_partial_failure() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let into = seed(&runtime, &token, &"x".repeat(MAX_TEXT_BYTES + 17)).await;
    let from = seed(&runtime, &token, "merged source remains tombstoned").await;
    register_pair(&runtime, Behavior::NonFinite);
    let fired = Arc::new(Mutex::new(Vec::new()));
    let seen = fired.clone();
    runtime.install_note_mutation_hook(Arc::new(move |_kind, id| {
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(id);
        })
    }));
    let summary = runtime
        .merge_note(
            &token,
            into.id,
            from.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::Append,
            false,
        )
        .await
        .expect("a committed partial merge returns its existing summary");
    assert_eq!(summary.kept_id, into.id);
    assert_eq!(summary.removed_id, from.id);
    let error = summary
        .post_commit_reindex_error
        .as_deref()
        .expect("merge must disclose model failure");
    assert!(error.contains("model broken vector validation"), "{error}");
    let updated = runtime
        .notes(&token)
        .unwrap()
        .get_note(into.id)
        .await
        .unwrap()
        .unwrap();
    assert!(updated.content.contains(&from.content));
    let discarded = (updated.content.len() - MAX_TEXT_BYTES) as u64;
    assert!(discarded > 0);
    assert!(
        error.contains(&format!("truncated=2 discarded_bytes={}", discarded * 2)),
        "{error}"
    );
    assert_eq!(
        summary.embedding_truncation.truncated, 2,
        "both actual service outcomes were bounded before validation/publication"
    );
    assert_eq!(summary.embedding_truncation.discarded_bytes, discarded * 2);
    assert_live_text(&runtime, &token, &updated).await;
    assert!(runtime
        .get_note_including_deleted(&token, from.id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    let events = runtime
        .events(&token)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![khive_types::EventKind::NoteMerged],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        events.items.len(),
        1,
        "merge event must remain committed with its rows"
    );
    assert_eq!(*fired.lock().unwrap(), vec![into.id]);
}

#[path = "note_reindex_hook_tests.rs"]
mod hook_tests;
