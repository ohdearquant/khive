use super::*;
use async_trait::async_trait;
use khive_runtime::{EmbedderProvider, RuntimeResult};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

const BROKEN: &str = "atomic-partial-broken";
const HEALTHY: &str = "atomic-partial-healthy";
const FAIL_INPUT: &str = "failure-marker";

struct Provider {
    name: &'static str,
    failing: bool,
    attempts: Arc<AtomicUsize>,
    drop_text_index: Option<KhiveRuntime>,
}

struct Service {
    failing: bool,
    attempts: Arc<AtomicUsize>,
    drop_text_index: Option<KhiveRuntime>,
}

#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        self.name
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(Service {
            failing: self.failing,
            attempts: self.attempts.clone(),
            drop_text_index: self.drop_text_index.clone(),
        }))
    }
}

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.attempts.fetch_add(texts.len(), Ordering::SeqCst);
        if let Some(runtime) = &self.drop_text_index {
            if texts.iter().any(|text| text.contains(FAIL_INPUT)) {
                // Removing the note text index after the marked note's own
                // lexical write makes every later note's lexical write fail.
                let mut writer = runtime.sql().writer().await.expect("fixture writer");
                writer
                    .execute_script("DROP TABLE IF EXISTS fts_notes".to_string())
                    .await
                    .expect("remove the note text index");
            }
        }
        if self.failing && texts.iter().any(|text| text.contains(FAIL_INPUT)) {
            return Err(EmbedError::Internal("injected atomic model failure".into()));
        }
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "atomic-partial-fixture"
    }
}

#[tokio::test]
async fn atomic_partial_note_indexing_discloses_failure_and_keeps_sibling_warning() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = RuntimeConfig {
        db_path: Some(dir.path().join("notes.db")),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    let (a, b) = {
        let rt = KhiveRuntime::new(cfg.clone()).unwrap();
        let tok = rt.authorize(Namespace::local()).unwrap();
        let a = rt
            .create_note(&tok, "observation", None, "old A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_note(&tok, "observation", None, "old B", None, None, vec![])
            .await
            .unwrap();
        (a, b)
    };
    let large = "x".repeat(MAX_TEXT_BYTES + 17);
    let updated_b = format!("new B {FAIL_INPUT}");
    let ops = [(a.id, large.clone()), (b.id, updated_b.clone())]
        .into_iter()
        .map(|(id, content)| OpsFileEntry {
            tool: "update".into(),
            args: json!({"id": id, "content": content, "embed": true}),
        })
        .collect();
    let broken_attempts = Arc::new(AtomicUsize::new(0));
    let healthy_attempts = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(Mutex::new(None));
    let capture = runtime.clone();
    let envelope = execute_atomic_ops_file_with_runtime_setup(
        ops,
        cfg,
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        |rt| {
            for (name, failing, attempts) in [
                (BROKEN, true, broken_attempts.clone()),
                (HEALTHY, false, healthy_attempts.clone()),
            ] {
                rt.register_embedder(Provider {
                    name,
                    failing,
                    attempts,
                    drop_text_index: None,
                });
            }
            *capture.lock().unwrap() = Some(rt.clone());
        },
    )
    .await
    .expect("a committed partial reindex returns its envelope");
    let rt = runtime.lock().unwrap().take().unwrap();
    let tok = rt.authorize(Namespace::local()).unwrap();
    for (original, expected) in [(&a, &large), (&b, &updated_b)] {
        let current = rt
            .notes(&tok)
            .unwrap()
            .get_note(original.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&current.content, expected);
        assert_eq!(current.version, original.version + 1);
        assert!(current.deleted_at.is_none());
        let text = rt
            .text_for_notes(&tok)
            .unwrap()
            .get_document("local", original.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&text.body, expected);
    }
    let good = rt
        .vectors_for_model(&tok, HEALTHY)
        .unwrap()
        .get_vectors(&[a.id, b.id], "local", "note.content")
        .await
        .unwrap();
    assert_eq!(
        good.len(),
        2,
        "healthy model must persist both committed notes"
    );
    let broken = rt
        .vectors_for_model(&tok, BROKEN)
        .unwrap()
        .get_vectors(&[a.id, b.id], "local", "note.content")
        .await
        .unwrap();
    assert!(broken.contains_key(&a.id));
    assert!(!broken.contains_key(&b.id));
    assert_eq!(
        broken_attempts.load(Ordering::SeqCst),
        2,
        "fault must execute on the real provider"
    );
    assert_eq!(healthy_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        envelope["results"][0]["result"]["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING])
    );
    assert!(envelope["results"][1]["result"].get("warnings").is_none());
    assert_eq!(
        envelope["summary"],
        json!({"total": 2, "succeeded": 2, "failed": 0})
    );
    assert!(envelope["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["ok"] == true));
    assert_eq!(envelope["atomic"]["committed"], true);
    assert_eq!(envelope["atomic"]["rolled_back"], false);
    assert!(envelope["atomic"]["failed_op_index"].is_null());
    assert!(envelope["atomic"]["error"].is_null());
    assert_eq!(
        envelope["atomic"]["status"], "committed_degraded",
        "partial indexing must be disclosed: {envelope}"
    );
    assert_eq!(envelope["atomic"]["retryable"], false);
    let failures = envelope["atomic"]["degradations"].as_array().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["stage"], "post_commit_reindex");
    let diagnostic = failures[0]["error"].as_str().unwrap();
    assert!(diagnostic.contains(&b.id.to_string()), "{diagnostic}");
    assert!(
        diagnostic.contains(&format!("model {BROKEN} embedding")),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains(HEALTHY),
        "healthy models must not be listed as failed: {diagnostic}"
    );
}

#[tokio::test]
async fn atomic_failed_effect_still_discloses_sibling_model_failure_and_warning() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = RuntimeConfig {
        db_path: Some(dir.path().join("notes.db")),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    let (a, b) = {
        let rt = KhiveRuntime::new(cfg.clone()).unwrap();
        let tok = rt.authorize(Namespace::local()).unwrap();
        let a = rt
            .create_note(&tok, "observation", None, "old A", None, None, vec![])
            .await
            .unwrap();
        let b = rt
            .create_note(&tok, "observation", None, "old B", None, None, vec![])
            .await
            .unwrap();
        (a, b)
    };
    // Op 0 (note B) is reindexed first: its lexical write succeeds, its bounded
    // text makes the healthy model embed a truncated input, and that embed
    // removes the note text index. The broken model then fails for B. Op 1
    // (note A) is reindexed next and fails closed at its lexical write.
    let long_b = format!("{FAIL_INPUT} {}", "x".repeat(MAX_TEXT_BYTES + 17));
    let ops = [(b.id, long_b), (a.id, "new A".to_string())]
        .into_iter()
        .map(|(id, content)| OpsFileEntry {
            tool: "update".into(),
            args: json!({"id": id, "content": content, "embed": true}),
        })
        .collect();
    let broken_attempts = Arc::new(AtomicUsize::new(0));
    let healthy_attempts = Arc::new(AtomicUsize::new(0));
    let envelope = execute_atomic_ops_file_with_runtime_setup(
        ops,
        cfg,
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        |rt| {
            for (name, failing, attempts, drop_text_index) in [
                (BROKEN, true, broken_attempts.clone(), None),
                (HEALTHY, false, healthy_attempts.clone(), Some(rt.clone())),
            ] {
                rt.register_embedder(Provider {
                    name,
                    failing,
                    attempts,
                    drop_text_index,
                });
            }
        },
    )
    .await
    .expect("a committed unit with a failed effect returns its envelope");
    assert_eq!(
        broken_attempts.load(Ordering::SeqCst),
        1,
        "only note B reaches its models; note A fails before embedding"
    );
    assert_eq!(healthy_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        envelope["summary"],
        json!({"total": 2, "succeeded": 2, "failed": 0})
    );
    assert!(envelope["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["ok"] == true));
    assert_eq!(envelope["atomic"]["committed"], true);
    assert_eq!(
        envelope["atomic"]["status"], "committed_degraded",
        "a failed effect and a failed model must both be disclosed: {envelope}"
    );
    let degradations = envelope["atomic"]["degradations"].as_array().unwrap();
    assert_eq!(degradations.len(), 2, "{envelope}");
    assert!(degradations
        .iter()
        .all(|degradation| degradation["stage"] == "post_commit_reindex"));
    let errors: Vec<&str> = degradations
        .iter()
        .map(|degradation| degradation["error"].as_str().unwrap())
        .collect();
    let failed_effect = errors
        .iter()
        .find(|error| error.contains("post-commit effects failed after commit"))
        .unwrap_or_else(|| panic!("the failed effect must be disclosed: {envelope}"));
    assert!(failed_effect.contains("effect[1]"), "{failed_effect}");
    assert!(!failed_effect.contains("effect[0]"), "{failed_effect}");
    assert!(failed_effect.contains(&a.id.to_string()), "{failed_effect}");
    assert!(
        !failed_effect.contains(&b.id.to_string()),
        "{failed_effect}"
    );
    let failed_model = errors
        .iter()
        .find(|error| error.contains(&format!("model {BROKEN} embedding")))
        .unwrap_or_else(|| panic!("the sibling model failure must be disclosed: {envelope}"));
    assert!(failed_model.contains(&b.id.to_string()), "{failed_model}");
    assert!(
        !failed_model.contains(HEALTHY),
        "healthy models must not be listed as failed: {failed_model}"
    );
    assert_eq!(
        envelope["results"][0]["result"]["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "the sibling truncation warning must survive the failed effect: {envelope}"
    );
    assert!(envelope["results"][1]["result"].get("warnings").is_none());
}
