use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use khive_storage::note::Note;
use khive_storage::types::{DeleteMode, SqlValue};
use khive_storage::SqlStatement;
use khive_types::{Details, ErrorKind, Namespace};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::embedder_registry::EmbedderProvider;
use crate::keyed_memory::{create_keyed_memory, validate_memory_key, KeyedMemorySpec};
use crate::operations::{arm_fts_fail_scoped, arm_vector_fail_scoped};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

const MODEL: &str = "keyed-memory-test-model";
const DIMS: usize = 4;
const CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(10);

struct TestEmbeddingService;

#[async_trait]
impl EmbeddingService for TestEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25; DIMS]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        MODEL
    }
}

struct TestEmbeddingProvider;

#[async_trait]
impl EmbedderProvider for TestEmbeddingProvider {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        DIMS
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(TestEmbeddingService))
    }
}

fn token(runtime: &KhiveRuntime, namespace: &str) -> NamespaceToken {
    runtime
        .authorize(Namespace::parse(namespace).expect("valid test namespace"))
        .expect("authorize test namespace")
}

async fn fixture(namespace: &str) -> (KhiveRuntime, NamespaceToken, Uuid) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.install_kind_registry(vec!["concept".into()], vec!["memory".into()]);
    let token = token(&runtime, namespace);
    let source = runtime
        .create_entity(&token, "concept", None, "memory source", None, None, vec![])
        .await
        .expect("create annotation source");
    runtime.register_embedder(TestEmbeddingProvider);
    runtime
        .vectors_for_model(&token, MODEL)
        .expect("test vector table");
    (runtime, token, source.id)
}

fn spec<'a>(key: &'a str, content: &'a str, source_id: Option<Uuid>) -> KeyedMemorySpec<'a> {
    KeyedMemorySpec {
        content,
        key,
        salience: 0.7,
        decay_factor: 0.95,
        properties: json!({"memory_type": "episodic", "tags": ["keyed-runtime"]}),
        source_id,
        embedding_model: None,
    }
}

async fn count(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> i64 {
    let mut reader = runtime.sql().reader().await.expect("sql reader");
    match reader
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params,
            label: Some("keyed-memory-test-count".into()),
        })
        .await
        .expect("count rows")
    {
        Some(SqlValue::Integer(value)) => value,
        other => panic!("unexpected count result: {other:?}"),
    }
}

async fn assert_rows(runtime: &KhiveRuntime, token: &NamespaceToken, notes: i64, edges: i64) {
    let namespace = token.namespace().as_str();
    for sql in [
        "SELECT COUNT(*) FROM notes WHERE namespace = ?1",
        "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1",
        "SELECT COUNT(*) FROM fts_notes_rowids WHERE namespace = ?1",
        "SELECT COUNT(*) FROM ann_write_log WHERE namespace = ?1",
    ] {
        assert_eq!(
            count(runtime, sql, vec![SqlValue::Text(namespace.into())]).await,
            notes,
            "unexpected row count for {sql}"
        );
    }
    assert_eq!(
        runtime
            .vectors_for_model(token, MODEL)
            .expect("test vector store")
            .count()
            .await
            .expect("count vectors"),
        notes as u64
    );
    assert_eq!(
        count(
            runtime,
            "SELECT COUNT(*) FROM graph_edges WHERE namespace = ?1",
            vec![SqlValue::Text(namespace.into())],
        )
        .await,
        edges
    );
}

fn assert_key_conflict(error: RuntimeError, key: &str, holder: Uuid) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected typed key conflict, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        error.details(),
        Some(&Details::new_owned([
            ("reason", "key_conflict".into()),
            ("key", key.to_owned()),
            ("existing_id", holder.to_string()),
        ]))
    );
}

#[tokio::test]
async fn keyed_memory_replay_keeps_holder_and_rolls_back_losing_indexes_and_edges() {
    let (runtime, token, source) = fixture("keyed-memory-replay").await;
    let (first, edge_id) = create_keyed_memory(
        &runtime,
        &token,
        spec("operation-one", "first memory content", Some(source)),
    )
    .await
    .expect("first keyed write");
    assert_eq!(first.key.as_deref(), Some("operation-one"));
    assert_eq!(first.salience, Some(0.7));
    assert_eq!(first.decay_factor, Some(0.95));
    assert_eq!(
        first.properties,
        Some(json!({
            "memory_type": "episodic", "tags": ["keyed-runtime"]
        }))
    );
    let edge_id = edge_id.expect("successful annotation is required");
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM graph_edges WHERE id = ?1 AND source_id = ?2 \
             AND target_id = ?3 AND relation = 'annotates' AND deleted_at IS NULL",
            vec![
                SqlValue::Text(edge_id.to_string()),
                SqlValue::Text(first.id.to_string()),
                SqlValue::Text(source.to_string()),
            ],
        )
        .await,
        1
    );
    assert_rows(&runtime, &token, 1, 1).await;

    let error = create_keyed_memory(
        &runtime,
        &token,
        spec("operation-one", "first memory content", Some(source)),
    )
    .await
    .expect_err("identical replay must name the holder");
    assert_key_conflict(error, "operation-one", first.id);
    assert_rows(&runtime, &token, 1, 1).await;

    let (decoy, _) = create_keyed_memory(
        &runtime,
        &token,
        spec(
            "different-operation",
            "decoy annotates the same source",
            Some(source),
        ),
    )
    .await
    .expect("different key may annotate the same source");
    assert_ne!(first.id, decoy.id);

    let error = create_keyed_memory(
        &runtime,
        &token,
        spec(
            "operation-one",
            "losing content must not survive",
            Some(source),
        ),
    )
    .await
    .expect_err("replay must be refused");
    assert_key_conflict(error, "operation-one", first.id);
    assert_rows(&runtime, &token, 2, 2).await;
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(first.id)
            .await
            .unwrap(),
        Some(first)
    );
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM notes WHERE content = ?1",
            vec![SqlValue::Text("losing content must not survive".into())],
        )
        .await,
        0
    );
}

#[tokio::test]
async fn keyed_memory_fts_and_vector_failures_roll_back_and_leave_key_available() {
    for (namespace, vector_fault) in [
        ("keyed-memory-fts-fault", false),
        ("keyed-memory-vector-fault", true),
    ] {
        let (runtime, token, source) = fixture(namespace).await;
        let arm = if vector_fault {
            arm_vector_fail_scoped(namespace)
        } else {
            arm_fts_fail_scoped(namespace)
        };
        let error = create_keyed_memory(
            &runtime,
            &token,
            spec("fault-operation", "rollback target", Some(source)),
        )
        .await
        .expect_err("injected statement must fail");
        let label = if vector_fault {
            "fault-injected-vector"
        } else {
            "fault-injected-fts"
        };
        assert!(
            error.to_string().contains(label),
            "unexpected failure: {error:?}"
        );
        drop(arm);
        assert_rows(&runtime, &token, 0, 0).await;
        let (_, edge) = create_keyed_memory(
            &runtime,
            &token,
            spec("fault-operation", "rollback target", Some(source)),
        )
        .await
        .expect("failed transaction must not claim the key");
        assert!(edge.is_some());
        assert_rows(&runtime, &token, 1, 1).await;
    }
}

#[tokio::test]
async fn keyed_memory_annotation_failure_rolls_back_all_prior_writes() {
    let (runtime, token, source) = fixture("keyed-memory-annotation-fault").await;
    let mut writer = runtime.sql().writer().await.expect("sql writer");
    writer
        .execute_script(
            "CREATE TRIGGER reject_keyed_annotation BEFORE INSERT ON graph_edges \
             WHEN NEW.relation = 'annotates' BEGIN \
             SELECT RAISE(ABORT, 'injected keyed annotation failure'); END;"
                .into(),
        )
        .await
        .expect("install scratch annotation failure trigger");
    drop(writer);
    let error = create_keyed_memory(
        &runtime,
        &token,
        spec(
            "annotation-operation",
            "annotation is required",
            Some(source),
        ),
    )
    .await
    .expect_err("failed annotation must fail the keyed create");
    assert!(
        error
            .to_string()
            .contains("injected keyed annotation failure"),
        "unexpected failure: {error:?}"
    );
    assert_rows(&runtime, &token, 0, 0).await;
    let mut writer = runtime.sql().writer().await.expect("sql writer");
    writer
        .execute_script("DROP TRIGGER reject_keyed_annotation;".into())
        .await
        .expect("remove scratch trigger");
    drop(writer);
    create_keyed_memory(
        &runtime,
        &token,
        spec(
            "annotation-operation",
            "annotation is required",
            Some(source),
        ),
    )
    .await
    .expect("failed annotation must leave the key available");
    assert_rows(&runtime, &token, 1, 1).await;
}

#[tokio::test]
async fn keyed_memory_missing_source_refuses_without_any_memory_effect() {
    let (runtime, token, _) = fixture("keyed-memory-missing-source").await;
    let missing = Uuid::new_v4();
    let error = create_keyed_memory(
        &runtime,
        &token,
        spec(
            "missing-source-operation",
            "missing annotation source",
            Some(missing),
        ),
    )
    .await
    .expect_err("missing source must be refused");
    assert!(
        matches!(&error, RuntimeError::NotFound(message) if message.contains(&missing.to_string()))
    );
    assert_rows(&runtime, &token, 0, 0).await;
}

#[tokio::test]
async fn keyed_memory_validates_key_bytes_before_writing_and_accepts_empty_key() {
    let (runtime, token, _) = fixture("keyed-memory-key-validation").await;
    let utf8_boundary = "\u{00e9}".repeat(256);
    assert_eq!(utf8_boundary.len(), 512);
    for invalid in [
        "x".repeat(513),
        format!("{utf8_boundary}x"),
        "nul\0key".into(),
    ] {
        assert!(matches!(
            validate_memory_key(&invalid),
            Err(RuntimeError::InvalidInput(_))
        ));
        let error = create_keyed_memory(&runtime, &token, spec(&invalid, "invalid key", None))
            .await
            .expect_err("invalid key must fail before writing");
        assert!(matches!(error, RuntimeError::InvalidInput(_)));
        assert_rows(&runtime, &token, 0, 0).await;
    }
    for key in ["", utf8_boundary.as_str()] {
        validate_memory_key(key).expect("valid byte-bounded key");
        let (note, edge) = create_keyed_memory(&runtime, &token, spec(key, "valid key", None))
            .await
            .expect("valid key must write");
        assert_eq!(note.key.as_deref(), Some(key));
        assert!(edge.is_none());
        let error = create_keyed_memory(&runtime, &token, spec(key, "valid key", None))
            .await
            .expect_err("valid key replay must conflict");
        assert_key_conflict(error, key, note.id);
    }
    assert_rows(&runtime, &token, 2, 0).await;
}

#[tokio::test]
async fn keyed_memory_same_key_coexists_in_distinct_namespace_tokens() {
    let (runtime, first_token, _) = fixture("keyed-memory-namespace-one").await;
    let second_token = token(&runtime, "keyed-memory-namespace-two");
    let (first, _) = create_keyed_memory(&runtime, &first_token, spec("shared", "one", None))
        .await
        .expect("first namespace write");
    let (second, _) = create_keyed_memory(&runtime, &second_token, spec("shared", "two", None))
        .await
        .expect("second namespace write");
    assert_ne!(first.id, second.id);
    assert_eq!(first.namespace, first_token.namespace().as_str());
    assert_eq!(second.namespace, second_token.namespace().as_str());
    for (token, holder) in [(&first_token, first.id), (&second_token, second.id)] {
        let error = create_keyed_memory(&runtime, token, spec("shared", "replay", None))
            .await
            .expect_err("each namespace must resolve its own holder");
        assert_key_conflict(error, "shared", holder);
        assert_rows(&runtime, token, 1, 0).await;
    }
}

#[derive(Debug)]
struct CheckpointEvent {
    attempt: usize,
    before_resolve: bool,
    resume: oneshot::Sender<()>,
}

type CheckpointHooks = HashMap<String, mpsc::UnboundedSender<CheckpointEvent>>;
static CHECKPOINT_HOOKS: OnceLock<Mutex<CheckpointHooks>> = OnceLock::new();

struct CheckpointArm {
    namespace: String,
}

impl Drop for CheckpointArm {
    fn drop(&mut self) {
        if let Some(hooks) = CHECKPOINT_HOOKS.get() {
            if let Ok(mut hooks) = hooks.lock() {
                hooks.remove(&self.namespace);
            }
        }
    }
}

fn arm_checkpoints(namespace: &str) -> (CheckpointArm, mpsc::UnboundedReceiver<CheckpointEvent>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let mut hooks = CHECKPOINT_HOOKS
        .get_or_init(Mutex::default)
        .lock()
        .expect("checkpoint hooks");
    assert!(
        !hooks.contains_key(namespace),
        "namespace checkpoint already armed"
    );
    hooks.insert(namespace.into(), sender);
    (
        CheckpointArm {
            namespace: namespace.into(),
        },
        receiver,
    )
}

pub(crate) async fn checkpoint(namespace: &str, attempt: usize, before_resolve: bool) {
    let sender = CHECKPOINT_HOOKS.get().and_then(|hooks| {
        hooks
            .lock()
            .expect("checkpoint hooks")
            .get(namespace)
            .cloned()
    });
    if let Some(sender) = sender {
        let (resume, resumed) = oneshot::channel();
        sender
            .send(CheckpointEvent {
                attempt,
                before_resolve,
                resume,
            })
            .expect("checkpoint receiver must remain alive while armed");
        tokio::time::timeout(CHECKPOINT_TIMEOUT, resumed)
            .await
            .expect("test must resume checkpoint before timeout")
            .expect("checkpoint resume sender dropped");
    }
}

async fn expect_checkpoint(
    receiver: &mut mpsc::UnboundedReceiver<CheckpointEvent>,
    attempt: usize,
    before_resolve: bool,
) -> oneshot::Sender<()> {
    let event = tokio::time::timeout(CHECKPOINT_TIMEOUT, receiver.recv())
        .await
        .expect("keyed helper must reach checkpoint before timeout")
        .expect("checkpoint channel closed");
    assert_eq!(
        (event.attempt, event.before_resolve),
        (attempt, before_resolve)
    );
    event.resume
}

// Store-only holders let the test change the real unique-index partition while
// the keyed helper is paused, without recursively entering its own checkpoint.
async fn seed_holder(runtime: &KhiveRuntime, token: &NamespaceToken, key: &str) -> Uuid {
    let mut note = Note::new(token.namespace().as_str(), "memory", "checkpoint holder");
    note.key = Some(key.into());
    let id = note.id;
    runtime
        .notes(token)
        .expect("note store")
        .upsert_note(note)
        .await
        .expect("seed holder");
    id
}

async fn remove_holder(runtime: &KhiveRuntime, token: &NamespaceToken, holder: Uuid) {
    assert!(runtime
        .notes(token)
        .expect("note store")
        .delete_note(holder, DeleteMode::Hard)
        .await
        .expect("remove holder"));
}

#[tokio::test]
async fn keyed_memory_disappearing_holder_retries_once_and_commits_one_candidate() {
    let (runtime, token, source) = fixture("keyed-memory-holder-retry").await;
    let key = "retry-operation";
    let holder = seed_holder(&runtime, &token, key).await;
    let (_arm, mut checkpoints) = arm_checkpoints(token.namespace().as_str());
    let worker_runtime = runtime.clone();
    let worker_token = token.clone();
    let worker = tokio::spawn(async move {
        create_keyed_memory(
            &worker_runtime,
            &worker_token,
            spec(key, "retry candidate", Some(source)),
        )
        .await
    });

    expect_checkpoint(&mut checkpoints, 0, false)
        .await
        .send(())
        .expect("resume first attempt");
    let resume = expect_checkpoint(&mut checkpoints, 0, true).await;
    remove_holder(&runtime, &token, holder).await;
    resume.send(()).expect("resume first holder lookup");
    expect_checkpoint(&mut checkpoints, 1, false)
        .await
        .send(())
        .expect("resume second attempt");
    let (note, edge) = tokio::time::timeout(CHECKPOINT_TIMEOUT, worker)
        .await
        .expect("worker must finish")
        .expect("worker must not panic")
        .expect("retry after holder disappearance must succeed");
    assert_ne!(note.id, holder);
    assert_eq!(note.key.as_deref(), Some(key));
    assert_eq!(note.content, "retry candidate");
    assert!(edge.is_some());
    assert!(
        checkpoints.try_recv().is_err(),
        "no third attempt or second holder lookup"
    );
    assert_rows(&runtime, &token, 1, 1).await;
}

#[tokio::test]
async fn keyed_memory_disappearing_holders_exhaust_exactly_two_attempts_without_candidate_rows() {
    let (runtime, token, source) = fixture("keyed-memory-holder-exhaustion").await;
    let key = "exhaustion-operation";
    let first_holder = seed_holder(&runtime, &token, key).await;
    let (_arm, mut checkpoints) = arm_checkpoints(token.namespace().as_str());
    let worker_runtime = runtime.clone();
    let worker_token = token.clone();
    let worker = tokio::spawn(async move {
        create_keyed_memory(
            &worker_runtime,
            &worker_token,
            spec(key, "exhausted candidate", Some(source)),
        )
        .await
    });

    expect_checkpoint(&mut checkpoints, 0, false)
        .await
        .send(())
        .expect("resume first attempt");
    let resume = expect_checkpoint(&mut checkpoints, 0, true).await;
    remove_holder(&runtime, &token, first_holder).await;
    resume.send(()).expect("resume first holder lookup");
    let resume = expect_checkpoint(&mut checkpoints, 1, false).await;
    let second_holder = seed_holder(&runtime, &token, key).await;
    assert_ne!(first_holder, second_holder);
    resume
        .send(())
        .expect("resume second attempt with replacement holder");
    let resume = expect_checkpoint(&mut checkpoints, 1, true).await;
    remove_holder(&runtime, &token, second_holder).await;
    resume.send(()).expect("resume second holder lookup");
    let error = tokio::time::timeout(CHECKPOINT_TIMEOUT, worker)
        .await
        .expect("worker must finish")
        .expect("worker must not panic")
        .expect_err("two vanished holders must exhaust the retry budget");
    let RuntimeError::Khive(error) = error else {
        panic!("expected named unresolved holder error, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.details(),
        Some(&Details::new_owned([
            ("reason", "key_holder_unresolved".into()),
            ("key", key.into()),
        ]))
    );
    assert!(checkpoints.try_recv().is_err(), "no third attempt");
    assert_rows(&runtime, &token, 0, 0).await;
}

#[tokio::test]
async fn keyed_memory_source_deleted_after_prepare_rolls_back_at_endpoint_guard() {
    let (runtime, token, source) = fixture("keyed-memory-source-disappears").await;
    let (_arm, mut checkpoints) = arm_checkpoints(token.namespace().as_str());
    let worker_runtime = runtime.clone();
    let worker_token = token.clone();
    let worker = tokio::spawn(async move {
        create_keyed_memory(
            &worker_runtime,
            &worker_token,
            spec(
                "source-disappearance",
                "prepared annotation candidate",
                Some(source),
            ),
        )
        .await
    });

    let resume = expect_checkpoint(&mut checkpoints, 0, false).await;
    assert!(runtime
        .delete_entity(&token, source, true)
        .await
        .expect("delete source"));
    assert!(!runtime
        .substrate_exists_by_id(&token, source)
        .await
        .expect("source lookup"));
    resume.send(()).expect("resume after source deletion");
    let error = tokio::time::timeout(CHECKPOINT_TIMEOUT, worker)
        .await
        .expect("worker must finish")
        .expect("worker must not panic")
        .expect_err("required annotation endpoint guard must refuse the transaction");
    assert!(
        matches!(&error, RuntimeError::Internal(message) if message.contains("GuardFailed")),
        "expected endpoint guard failure, not key conflict: {error:?}"
    );
    assert!(
        checkpoints.try_recv().is_err(),
        "endpoint failure must not retry or resolve a key"
    );
    assert_rows(&runtime, &token, 0, 0).await;
}

#[tokio::test]
async fn keyed_memory_checkpoint_arms_are_namespace_scoped_and_removed_on_drop() {
    let namespace = "keyed-memory-checkpoint-scope";
    let (arm, mut checkpoints) = arm_checkpoints(namespace);
    checkpoint("keyed-memory-unarmed-namespace", 0, false).await;
    assert!(checkpoints.try_recv().is_err());
    drop(arm);
    checkpoint(namespace, 0, false).await;
    assert!(checkpoints.try_recv().is_err());
}
