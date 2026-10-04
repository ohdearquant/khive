use super::*;
use crate::embedder_registry::EmbedderProvider;
use crate::RuntimeError;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone, Copy)]
enum Fault {
    Provider,
    NonFinite,
    Store,
    Publication,
    None,
}

struct Provider {
    name: &'static str,
    fault: Fault,
    attempts: Arc<AtomicUsize>,
}
struct Service(Fault);

#[async_trait::async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        self.name
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if matches!(self.fault, Fault::Provider) {
            return Err(RuntimeError::Internal("injected provider failure".into()));
        }
        Ok(Arc::new(Service(self.fault)))
    }
}

#[async_trait::async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let value = if matches!(self.0, Fault::NonFinite) {
            f32::NAN
        } else {
            0.5
        };
        Ok(texts.iter().map(|_| vec![value; 4]).collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "note-report-fixture"
    }
}

async fn fixture(
    fault: Fault,
) -> (
    KhiveRuntime,
    NamespaceToken,
    Note,
    EmbeddingModelPlan,
    Arc<AtomicUsize>,
) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let attempts = Arc::new(AtomicUsize::new(0));
    for (name, fault) in [("broken", fault), ("healthy", Fault::None)] {
        runtime.register_embedder(Provider {
            name,
            fault,
            attempts: attempts.clone(),
        });
    }
    let note = Note::new("local", "observation", "current note report text");
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let mut plan = EmbeddingModelPlan::capture(&runtime);
    plan.model_names.sort();
    assert_eq!(plan.model_names, vec!["broken", "healthy"]);
    (runtime, token, note, plan, attempts)
}

async fn assert_report(fault: Fault, stage: ReindexModelStage) {
    let (runtime, token, note, plan, attempts) = fixture(fault).await;
    runtime.vectors_for_model(&token, "healthy").unwrap();
    if matches!(fault, Fault::Publication) {
        runtime.vectors_for_model(&token, "broken").unwrap();
    }
    let sql_attempts = Arc::new(AtomicUsize::new(0));
    if matches!(fault, Fault::Store | Fault::Publication) {
        let observed = sql_attempts.clone();
        let writer = runtime.backend().pool().writer().unwrap();
        writer.conn().authorizer(Some(move |context: AuthContext<'_>| {
            let deny = match fault {
                Fault::Store => matches!(context.action, AuthAction::CreateVtable { table_name, .. } if table_name == "vec_broken"),
                Fault::Publication => matches!(context.action, AuthAction::Insert { table_name } if table_name == "vec_broken"),
                _ => false,
            };
            if deny { observed.fetch_add(1, Ordering::SeqCst); Authorization::Deny } else { Authorization::Allow }
        })).unwrap();
    }
    let report = runtime
        .reindex_note_report_with_plan(&token, &note, &plan)
        .await
        .expect("partial vector failure remains report data");
    if matches!(fault, Fault::Store | Fault::Publication) {
        let writer = runtime.backend().pool().writer().unwrap();
        writer
            .conn()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert!(
            sql_attempts.load(Ordering::SeqCst) > 0,
            "real selected SQL fault must execute"
        );
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "healthy model after the failing model must execute"
    );
    let current = runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current, note);
    let text = runtime
        .text_for_notes(&token)
        .unwrap()
        .get_document("local", note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text.body, note.content);
    let healthy = runtime
        .vectors_for_model(&token, "healthy")
        .unwrap()
        .get_vectors(&[note.id], "local", "note.content")
        .await
        .unwrap();
    assert_eq!(healthy.get(&note.id), Some(&vec![0.5; 4]));
    assert_eq!(
        report.failures.len(),
        1,
        "one failed model must be retained in the typed report"
    );
    assert_eq!(report.failures[0].model, "broken");
    assert_eq!(report.failures[0].stage, stage);
    assert!(!report.failures[0].error.is_empty());
    assert_eq!(report.truncation, EmbeddingTruncationReport::default());
}

#[tokio::test]
async fn report_retains_provider_failure_and_healthy_model() {
    assert_report(Fault::Provider, ReindexModelStage::Embedding).await;
}

#[tokio::test]
async fn report_retains_nonfinite_failure_and_healthy_model() {
    assert_report(Fault::NonFinite, ReindexModelStage::VectorValidation).await;
}

#[tokio::test]
async fn report_retains_vector_store_failure_and_healthy_model() {
    assert_report(Fault::Store, ReindexModelStage::VectorStore).await;
}

#[tokio::test]
async fn report_retains_vector_publication_failure_and_healthy_model() {
    assert_report(Fault::Publication, ReindexModelStage::VectorPublication).await;
}

#[tokio::test]
async fn legacy_reindex_keeps_embedding_failure_success() {
    let (runtime, token, note, plan, _) = fixture(Fault::Provider).await;
    runtime
        .reindex_note_with_plan(&token, &note, &plan)
        .await
        .expect("legacy reindex remains best effort for vectors");
}

#[tokio::test]
async fn legacy_post_commit_effects_keep_embedding_failure_success() {
    let (runtime, token, note, _, attempts) = fixture(Fault::Provider).await;
    let plan = crate::atomic_prepare::prepare_update(
        &runtime,
        &token,
        &serde_json::json!({"id": note.id, "content": "legacy committed update", "embed": true}),
        None,
    )
    .await
    .unwrap();
    let outcome = crate::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .unwrap();
    let crate::AtomicRunOutcome::Committed { post_commit } = outcome else {
        panic!("real atomic note update must commit");
    };
    crate::atomic_prepare::apply_post_commit_effects(&runtime, &token, post_commit)
        .await
        .expect("legacy effect consumers retain partial embedding success");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let current = runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.content, "legacy committed update");
    assert_eq!(current.version, note.version + 1);
}

#[tokio::test]
async fn report_fts_failure_remains_an_error() {
    let (runtime, token, note, plan, attempts) = fixture(Fault::None).await;
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script("DROP TABLE fts_notes".into())
        .await
        .unwrap();
    runtime
        .reindex_note_report_with_plan(&token, &note, &plan)
        .await
        .expect_err("FTS failure must not become a partial vector report");
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap(),
        note
    );
}
