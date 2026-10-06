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
use crate::keyed_memory::{
    create_keyed_memory, create_keyed_memory_with_receipt, validate_memory_key, KeyedMemorySpec,
};
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
        "SELECT COUNT(*) FROM memory_visibility_receipts WHERE namespace = ?1",
        "SELECT COUNT(*) FROM memory_visibility_fences WHERE namespace = ?1",
        "SELECT COUNT(*) FROM memory_visibility_epochs WHERE namespace = ?1",
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

#[tokio::test]
async fn keyed_memory_without_models_replays_an_explicit_empty_receipt() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = token(&runtime, "keyed-memory-zero-model-receipt");
    let (first, _, replayed, fences) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("zero-model-key", "text-only keyed memory", None),
    )
    .await
    .expect("first keyed write");
    assert!(!replayed);
    assert!(fences.is_empty());
    assert_eq!(
        count(
            &runtime,
            "SELECT model_count FROM memory_visibility_receipts WHERE namespace = ?1 AND note_id = ?2",
            vec![
                SqlValue::Text(token.namespace().as_str().into()),
                SqlValue::Text(first.id.to_string()),
            ],
        )
        .await,
        0
    );

    let (second, _, was_replay, replay_fences) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("zero-model-key", "text-only keyed memory", None),
    )
    .await
    .expect("exact replay");
    assert_eq!(second.id, first.id);
    assert!(was_replay);
    assert!(replay_fences.is_empty());
}

#[tokio::test]
async fn keyed_memory_replay_preserves_exact_vector_fence_after_log_compaction() {
    let (runtime, token, _) = fixture("keyed-memory-visibility-replay").await;
    let (first, _, replayed, fences) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec(
            "visibility-key",
            "distinctive keyed visibility memory",
            None,
        ),
    )
    .await
    .expect("first keyed write");
    assert!(!replayed);
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].0, MODEL);
    let original_seq = count(
        &runtime,
        "SELECT seq FROM ann_write_log WHERE subject_id = ?1 AND embedding_model = ?2 AND op = 'upsert'",
        vec![
            SqlValue::Text(first.id.to_string()),
            SqlValue::Text(MODEL.into()),
        ],
    )
    .await;
    assert_eq!(fences[0].1, original_seq as u64);

    // A compacted log no longer identifies the original write. The receipt
    // sidecar must keep the old sequence without writing a replacement row.
    runtime
        .sql()
        .writer()
        .await
        .expect("writer")
        .execute(SqlStatement {
            sql: "DELETE FROM ann_write_log WHERE namespace = ?1 AND subject_id = ?2".into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().into()),
                SqlValue::Text(first.id.to_string()),
            ],
            label: Some("test-compact-original-memory-log".into()),
        })
        .await
        .expect("compact original log row");
    let (second, _, was_replay, replay_fences) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec(
            "visibility-key",
            "distinctive keyed visibility memory",
            None,
        ),
    )
    .await
    .expect("exact replay");
    assert_eq!(second.id, first.id);
    assert!(was_replay);
    assert_eq!(replay_fences, fences);
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM ann_write_log WHERE namespace = ?1 AND subject_id = ?2",
            vec![
                SqlValue::Text(token.namespace().as_str().into()),
                SqlValue::Text(first.id.to_string()),
            ],
        )
        .await,
        0,
        "exact replay must not mint another vector log row"
    );
}

#[tokio::test]
async fn keyed_memory_replay_refuses_when_original_receipt_is_missing() {
    let (runtime, token, _) = fixture("keyed-memory-missing-visibility-receipt").await;
    let (first, _, _, _) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("missing-receipt-key", "memory with a lost receipt", None),
    )
    .await
    .expect("first keyed write");
    runtime
        .sql()
        .writer()
        .await
        .expect("writer")
        .execute(SqlStatement {
            sql: "DELETE FROM memory_visibility_receipts WHERE namespace = ?1 AND note_id = ?2"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().into()),
                SqlValue::Text(first.id.to_string()),
            ],
            label: Some("test-remove-original-visibility-receipt".into()),
        })
        .await
        .expect("remove receipt");

    let error = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("missing-receipt-key", "memory with a lost receipt", None),
    )
    .await
    .expect_err("replay cannot invent a newer fence");
    let RuntimeError::Khive(error) = error else {
        panic!("expected typed unmet receipt, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.details().and_then(|details| details.get("reason")),
        Some("receipt_temporarily_unavailable")
    );
}

#[tokio::test]
async fn keyed_memory_replay_refuses_when_original_model_fence_is_missing() {
    let (runtime, token, _) = fixture("keyed-memory-missing-model-fence").await;
    let (first, _, _, _) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("missing-fence-key", "memory with a lost model fence", None),
    )
    .await
    .expect("first keyed write");
    runtime
        .sql()
        .writer()
        .await
        .expect("writer")
        .execute(SqlStatement {
            sql: "DELETE FROM memory_visibility_fences WHERE namespace = ?1 AND note_id = ?2"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().into()),
                SqlValue::Text(first.id.to_string()),
            ],
            label: Some("test-remove-original-model-fence".into()),
        })
        .await
        .expect("remove model fence");

    let error = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("missing-fence-key", "memory with a lost model fence", None),
    )
    .await
    .expect_err("replay cannot present a zero-model receipt for a model write");
    let RuntimeError::Khive(error) = error else {
        panic!("expected typed unmet receipt, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert_eq!(
        error.details().and_then(|details| details.get("reason")),
        Some("receipt_temporarily_unavailable")
    );
}

fn assert_idempotency_conflict(error: RuntimeError, key: &str, holder: Uuid) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected typed key conflict, got {error:?}");
    };
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        error.details(),
        Some(&Details::new_owned([
            ("reason", "idempotency_key_conflict".into()),
            ("key", key.to_owned()),
            ("existing_id", holder.to_string()),
        ]))
    );
}

#[tokio::test]
async fn keyed_memory_replay_keeps_holder_and_rolls_back_losing_indexes_and_edges() {
    let (runtime, token, source) = fixture("keyed-memory-replay").await;
    let (first, edge_id, _) = create_keyed_memory(
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

    let (replayed, replay_edge, was_replay) = create_keyed_memory(
        &runtime,
        &token,
        spec("operation-one", "first memory content", Some(source)),
    )
    .await
    .expect("identical replay must return the holder");
    assert_eq!(replayed.id, first.id);
    assert!(replay_edge.is_none());
    assert!(was_replay);
    assert_rows(&runtime, &token, 1, 1).await;

    let (decoy, _, _) = create_keyed_memory(
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
    assert_idempotency_conflict(error, "operation-one", first.id);
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
        let (_, edge, _) = create_keyed_memory(
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
        let (note, edge, _) = create_keyed_memory(&runtime, &token, spec(key, "valid key", None))
            .await
            .expect("valid key must write");
        assert_eq!(note.key.as_deref(), Some(key));
        assert!(edge.is_none());
        let (replayed, replay_edge, was_replay) =
            create_keyed_memory(&runtime, &token, spec(key, "valid key", None))
                .await
                .expect("valid key replay must return the holder");
        assert_eq!(replayed.id, note.id);
        assert!(replay_edge.is_none());
        assert!(was_replay);
    }
    assert_rows(&runtime, &token, 2, 0).await;
}

#[tokio::test]
async fn keyed_memory_same_key_coexists_in_distinct_namespace_tokens() {
    let (runtime, first_token, _) = fixture("keyed-memory-namespace-one").await;
    let second_token = token(&runtime, "keyed-memory-namespace-two");
    let (first, _, _) = create_keyed_memory(&runtime, &first_token, spec("shared", "one", None))
        .await
        .expect("first namespace write");
    let (second, _, _) = create_keyed_memory(&runtime, &second_token, spec("shared", "two", None))
        .await
        .expect("second namespace write");
    assert_ne!(first.id, second.id);
    assert_eq!(first.namespace, first_token.namespace().as_str());
    assert_eq!(second.namespace, second_token.namespace().as_str());
    for (token, holder) in [(&first_token, first.id), (&second_token, second.id)] {
        let error = create_keyed_memory(&runtime, token, spec("shared", "replay", None))
            .await
            .expect_err("each namespace must resolve its own holder");
        assert_idempotency_conflict(error, "shared", holder);
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
    let (note, edge, replayed) = tokio::time::timeout(CHECKPOINT_TIMEOUT, worker)
        .await
        .expect("worker must finish")
        .expect("worker must not panic")
        .expect("retry after holder disappearance must succeed");
    assert!(!replayed);
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

async fn receipt_test_execute(runtime: &KhiveRuntime, sql: &str, id: Uuid) {
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: sql.into(),
            params: vec![SqlValue::Text(id.to_string())],
            label: Some("receipt-provenance-control".into()),
        })
        .await
        .unwrap();
}

async fn receipt_snapshot(runtime: &KhiveRuntime) -> serde_json::Value {
    let mut reader = runtime.sql().reader().await.unwrap();
    let mut snapshots = Vec::new();
    for table in [
        "notes",
        "memory_visibility_epochs",
        "memory_visibility_receipts",
        "memory_visibility_fences",
        "ann_write_log",
        "graph_edges",
    ] {
        snapshots.push(
            reader
                .query_all(SqlStatement {
                    sql: format!("SELECT * FROM {table} ORDER BY rowid"),
                    params: vec![],
                    label: Some("receipt-snapshot".into()),
                })
                .await
                .unwrap(),
        );
    }
    serde_json::to_value(snapshots).unwrap()
}

async fn assert_unknown_replay(runtime: &KhiveRuntime, token: &NamespaceToken, note: &Note) {
    let before = receipt_snapshot(runtime).await;
    for _ in 0..2 {
        let error = create_keyed_memory_with_receipt(
            runtime,
            token,
            spec(note.key.as_deref().unwrap(), &note.content, None),
        )
        .await
        .unwrap_err();
        let value =
            crate::error_projection::runtime_error_value(error, crate::DomainDisposition::Unknown);
        assert_eq!(value["details"]["reason"], "receipt_epoch_unknown");
        assert_eq!(value["details"]["memory_id"], note.id.to_string());
        assert_eq!(value["retryable"], false);
        assert_eq!(value["domain_disposition"], "not_committed");
        assert_eq!(receipt_snapshot(runtime).await, before);
    }
}

#[tokio::test]
async fn lower_note_constructors_and_statement_builders_never_assert_modern() {
    for route in 0..8 {
        let runtime = KhiveRuntime::memory().unwrap();
        runtime.install_kind_registry(vec![], vec!["memory".into()]);
        let token = token(&runtime, "receipt-lower-writer");
        let mut note = Note::new(token.namespace().as_str(), "memory", "lower-level identity");
        note.key = Some("lower-key".into());
        let store = runtime.notes(&token).unwrap();
        match route {
            0 => store.upsert_note(note.clone()).await.unwrap(),
            1 => assert!(store.insert_note_if_absent(note.clone()).await.unwrap()),
            2 => assert!(store.try_insert_note(note.clone()).await.unwrap()),
            3 => assert!(runtime
                .backend()
                .notes()
                .unwrap()
                .try_insert_note_with_attachments(note.clone(), vec![])
                .await
                .unwrap()),
            4 => {
                store.upsert_notes(vec![note.clone()]).await.unwrap();
            }
            5..=7 => {
                let statement = match route {
                    5 => khive_db::stores::note::note_upsert_statement(&note),
                    6 => khive_db::stores::note::note_insert_if_absent_statement(&note),
                    _ => khive_db::stores::note::note_insert_keyed_statement(&note),
                };
                runtime
                    .sql()
                    .writer()
                    .await
                    .unwrap()
                    .execute(statement)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            count(
                &runtime,
                "SELECT COUNT(*) FROM memory_visibility_epochs WHERE note_id = ?1",
                vec![SqlValue::Text(note.id.to_string())]
            )
            .await,
            0
        );
        assert_unknown_replay(&runtime, &token, &note).await;
    }
}

#[tokio::test]
async fn in_place_kind_change_and_restored_receipt_cannot_promote_unknown() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["memory".into(), "observation".into()]);
    let token = token(&runtime, "receipt-kind-change");
    let mut note = Note::new(
        token.namespace().as_str(),
        "observation",
        "later memory identity",
    );
    note.key = Some("kind-change-key".into());
    // The low-level constructor, unlike the runtime policy wrapper, accepts a
    // caller-supplied replacement kind. It still has no receipt context.
    let store = runtime.backend().notes().unwrap();
    store.upsert_note(note.clone()).await.unwrap();
    note.kind = "memory".into();
    store.upsert_note(note.clone()).await.unwrap();
    assert_unknown_replay(&runtime, &token, &note).await;

    let (modern, _, _, _) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("unknown-key", "complete but unknown", None),
    )
    .await
    .unwrap();
    receipt_test_execute(
        &runtime,
        "UPDATE memory_visibility_epochs SET epoch = 'unknown' WHERE note_id = ?1",
        modern.id,
    )
    .await;
    // A complete original header is already present. It cannot promote unknown
    // either now or after that same zero-model receipt is restored.
    assert_unknown_replay(&runtime, &token, &modern).await;
    receipt_test_execute(
        &runtime,
        "DELETE FROM memory_visibility_receipts WHERE note_id = ?1",
        modern.id,
    )
    .await;
    receipt_test_execute(&runtime, "INSERT INTO memory_visibility_receipts(namespace, note_id, model_count) SELECT namespace, id, 0 FROM notes WHERE id = ?1", modern.id).await;
    assert_unknown_replay(&runtime, &token, &modern).await;
}

#[tokio::test]
async fn contradictory_receipt_namespace_and_unreadable_epoch_store_do_not_issue_fences() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = token(&runtime, "receipt-conflict");
    let (note, _, _, _) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("conflict-key", "receipt identity", None),
    )
    .await
    .unwrap();
    receipt_test_execute(
        &runtime,
        "UPDATE memory_visibility_receipts SET namespace = 'foreign' WHERE note_id = ?1",
        note.id,
    )
    .await;
    assert_unknown_replay(&runtime, &token, &note).await;
    receipt_test_execute(&runtime, "UPDATE memory_visibility_receipts SET namespace = (SELECT namespace FROM notes WHERE id = ?1) WHERE note_id = ?1", note.id).await;
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "ALTER TABLE memory_visibility_epochs RENAME TO unavailable_epochs".into(),
            params: vec![],
            label: Some("receipt-unavailable-control".into()),
        })
        .await
        .unwrap();
    let error = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("conflict-key", "receipt identity", None),
    )
    .await
    .unwrap_err();
    let value =
        crate::error_projection::runtime_error_value(error, crate::DomainDisposition::Unknown);
    assert_eq!(value["details"]["reason"], "receipt_store_unavailable");
    assert_eq!(value["retryable"], true);
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM unavailable_epochs WHERE note_id = ?1 AND epoch = 'modern'",
            vec![SqlValue::Text(note.id.to_string())]
        )
        .await,
        1
    );
}

#[tokio::test]
async fn generic_and_stream_atomic_preparation_publish_modern_with_the_note() {
    use crate::atomic_message::{
        prepare_atomic_note_requests, AtomicNoteOptions, AtomicNoteRequest, AtomicNoteSpec,
    };
    use crate::atomic_runner::{run_atomic_unit, AtomicRunOutcome};
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = token(&runtime, "receipt-shared-atomic-writer");
    // This is the shared production preparer used by generic keyed creates and
    // stream-batch members; no specialized memory flag is supplied here.
    let prepared = prepare_atomic_note_requests(
        &runtime,
        ["generic-key", "stream-key"]
            .into_iter()
            .map(|key| AtomicNoteRequest {
                spec: AtomicNoteSpec {
                    token: &token,
                    id: None,
                    kind: "memory",
                    name: None,
                    content: key,
                    properties: None,
                },
                options: AtomicNoteOptions {
                    key: Some(key),
                    ..Default::default()
                },
            })
            .collect(),
    )
    .await
    .unwrap();
    let ids: Vec<_> = prepared.notes.iter().map(|note| note.id).collect();
    assert!(matches!(
        run_atomic_unit(runtime.sql().as_ref(), prepared.plans)
            .await
            .unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    for id in ids {
        assert_eq!(count(&runtime, "SELECT COUNT(*) FROM memory_visibility_epochs e JOIN memory_visibility_receipts r ON r.note_id = e.note_id AND r.namespace = e.namespace JOIN notes n ON n.id = e.note_id WHERE n.id = ?1 AND n.key IS NOT NULL AND e.epoch = 'modern' AND r.model_count = 0", vec![SqlValue::Text(id.to_string())]).await, 1);
    }
}

#[tokio::test]
async fn modern_epoch_follows_real_delete_restore_and_new_identity_on_key_reuse() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = token(&runtime, "receipt-real-lifecycle");
    let (original, _, _, original_fences) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("lifecycle-key", "lifecycle memory", None),
    )
    .await
    .unwrap();
    assert!(runtime
        .delete_note(&token, original.id, false)
        .await
        .unwrap());
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM memory_visibility_epochs WHERE note_id = ?1 AND epoch = 'modern'",
            vec![SqlValue::Text(original.id.to_string())]
        )
        .await,
        1
    );
    assert!(
        runtime
            .restore_note(&token, original.id)
            .await
            .unwrap()
            .unwrap()
            .1
    );
    let (restored, _, replay, fences) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("lifecycle-key", "lifecycle memory", None),
    )
    .await
    .unwrap();
    assert_eq!(restored.id, original.id);
    assert!(replay);
    assert_eq!(fences, original_fences);
    assert!(runtime
        .delete_note(&token, original.id, true)
        .await
        .unwrap());
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM memory_visibility_epochs WHERE note_id = ?1",
            vec![SqlValue::Text(original.id.to_string())]
        )
        .await,
        0
    );
    let (replacement, _, replay, _) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("lifecycle-key", "replacement identity", None),
    )
    .await
    .unwrap();
    assert!(!replay);
    assert_ne!(replacement.id, original.id);
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM memory_visibility_epochs WHERE note_id = ?1 AND epoch = 'modern'",
            vec![SqlValue::Text(replacement.id.to_string())]
        )
        .await,
        1
    );
}

#[tokio::test]
async fn stream_batch_keyed_memory_records_modern_without_the_specialized_writer() {
    use crate::streams::{StreamBatchMember, StreamWriteSpec};
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = token(&runtime, "receipt-stream-writer");
    let registry = crate::pack::VerbRegistryBuilder::new().build().unwrap();
    let result = runtime
        .stream_batch_atomic(
            &token,
            vec![StreamBatchMember::Write(StreamWriteSpec {
                key: "stream-memory-key".into(),
                kind: "memory".into(),
                doc: json!({"memory": "batched"}),
                tags: None,
                embed: Some(false),
                expected_version: None,
            })],
            None,
            vec![],
            &registry,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.len(), 1);
    let notes = runtime
        .notes(&token)
        .unwrap()
        .get_live_notes_by_key(
            token.namespace().as_str(),
            "stream-memory-key",
            Some("memory"),
        )
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(count(&runtime, "SELECT COUNT(*) FROM memory_visibility_epochs e JOIN memory_visibility_receipts r ON e.note_id = r.note_id AND e.namespace = r.namespace WHERE e.note_id = ?1 AND e.epoch = 'modern' AND r.model_count = 0", vec![SqlValue::Text(notes[0].id.to_string())]).await, 1);
}

#[tokio::test]
async fn note_merge_does_not_transfer_epoch_to_the_existing_destination_identity() {
    use crate::curation::{ContentMergeStrategy, EntityDedupMergePolicy};
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["memory".into()]);
    let token = token(&runtime, "receipt-merge-identity");
    let (source, _, _, _) = create_keyed_memory_with_receipt(
        &runtime,
        &token,
        spec("merge-source", "source content", None),
    )
    .await
    .unwrap();
    let mut destination = Note::new(token.namespace().as_str(), "memory", "destination content");
    destination.key = Some("merge-destination".into());
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(destination.clone())
        .await
        .unwrap();
    runtime
        .merge_note(
            &token,
            destination.id,
            source.id,
            EntityDedupMergePolicy::PreferInto,
            ContentMergeStrategy::PreferInto,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM memory_visibility_epochs WHERE note_id = ?1",
            vec![SqlValue::Text(destination.id.to_string())]
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &runtime,
            "SELECT COUNT(*) FROM memory_visibility_epochs WHERE note_id = ?1 AND epoch = 'modern'",
            vec![SqlValue::Text(source.id.to_string())]
        )
        .await,
        1
    );
    let destination = runtime
        .notes(&token)
        .unwrap()
        .get_note(destination.id)
        .await
        .unwrap()
        .unwrap();
    assert_unknown_replay(&runtime, &token, &destination).await;
}

#[tokio::test]
async fn orphan_fences_are_unknown_even_with_a_legacy_or_modern_marker() {
    for epoch in ["legacy", "modern"] {
        let runtime = KhiveRuntime::memory().unwrap();
        runtime.install_kind_registry(vec![], vec!["memory".into()]);
        let token = token(&runtime, "receipt-orphan-fence");
        let (note, _, _, _) = create_keyed_memory_with_receipt(
            &runtime,
            &token,
            spec("orphan-key", "orphan receipt evidence", None),
        )
        .await
        .unwrap();
        // Model a damaged historical database without changing the reader:
        // the fence has no header but retains its exact note attribution.
        {
            let writer = runtime.backend().pool().try_writer().unwrap();
            writer
                .conn()
                .pragma_update(None, "foreign_keys", false)
                .unwrap();
            writer
                .conn()
                .execute(
                    "DELETE FROM memory_visibility_receipts WHERE note_id = ?1",
                    [note.id.to_string()],
                )
                .unwrap();
            writer.conn().execute("INSERT INTO memory_visibility_fences(namespace, note_id, model, ann_write_log_seq) VALUES (?1, ?2, 'original-model', 7)", rusqlite::params![token.namespace().as_str(), note.id.to_string()]).unwrap();
            writer
                .conn()
                .execute(
                    "UPDATE memory_visibility_epochs SET epoch = ?1 WHERE note_id = ?2",
                    rusqlite::params![epoch, note.id.to_string()],
                )
                .unwrap();
            writer
                .conn()
                .pragma_update(None, "foreign_keys", true)
                .unwrap();
        }
        assert_unknown_replay(&runtime, &token, &note).await;
    }
}
