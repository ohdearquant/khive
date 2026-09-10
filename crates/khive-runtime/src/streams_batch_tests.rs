use super::*;
use async_trait::async_trait;
use khive_storage::{SqlReader, StorageError, StorageResult, WriterTaskRequestState};
use khive_types::{HandlerDef, Namespace, Pack};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use std::sync::{Arc, Mutex};

use crate::embedder_registry::EmbedderProvider;
use crate::pack::{KindHook, PackRuntime, VerbRegistryBuilder};

const MODEL: &str = "stream-batch-test";

struct Service;

#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        MODEL
    }
}

struct Provider;

#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(Service))
    }
}

struct CountingService(Arc<Mutex<Vec<Vec<String>>>>);

#[async_trait]
impl EmbeddingService for CountingService {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.0.lock().unwrap().push(texts.to_vec());
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        MODEL
    }
}

struct CountingProvider(Arc<Mutex<Vec<Vec<String>>>>);

#[async_trait]
impl EmbedderProvider for CountingProvider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(CountingService(self.0.clone())))
    }
}

fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(
        vec![],
        vec!["head".into(), "observation".into(), "normalized".into()],
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime.register_embedder(Provider);
    runtime.vectors_for_model(&token, MODEL).unwrap();
    (runtime, token, VerbRegistryBuilder::new().build().unwrap())
}

fn write(key: &str, version: Option<i64>) -> StreamWriteSpec {
    StreamWriteSpec {
        key: key.into(),
        kind: "head".into(),
        doc: json!({"revision": version.unwrap_or(0)}),
        tags: None,
        embed: None,
        expected_version: version,
    }
}

fn append(stream: &str, expected_seq: Option<i64>) -> StreamBatchMember {
    StreamBatchMember::Append(StreamAppendSpec {
        stream: stream.into(),
        record: json!({"event": "batch"}),
        expected_seq,
        note_kind: "observation".into(),
        tags: None,
        fence: None,
    })
}

fn fenced_append(stream: &str, fences: Vec<NoteFence>) -> StreamBatchMember {
    let StreamBatchMember::Append(mut spec) = append(stream, None) else {
        unreachable!()
    };
    spec.fence = Some(NoteFences::Many(fences));
    StreamBatchMember::Append(spec)
}

async fn batch_write(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    spec: StreamWriteSpec,
) -> Value {
    runtime
        .stream_batch_atomic(
            token,
            vec![StreamBatchMember::Write(spec)],
            None,
            vec![],
            registry,
        )
        .await
        .unwrap()
        .unwrap()
        .remove(0)
}

async fn vectors(runtime: &KhiveRuntime, token: &NamespaceToken) -> u64 {
    runtime
        .vectors_for_model(token, MODEL)
        .unwrap()
        .count()
        .await
        .unwrap()
}

#[tokio::test]
async fn stream_batch_embeds_distinct_appends_and_eligible_creates_in_one_call() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_kind_registry(vec![], vec!["head".into(), "observation".into()]);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    runtime.register_embedder(CountingProvider(calls.clone()));
    runtime.embedder_with_token(&token, MODEL).await.unwrap();
    let registry = VerbRegistryBuilder::new().build().unwrap();
    let mut members = vec![append("embedding", None)];
    for (key, kind, embed) in [
        ("first", "observation", None),
        ("second", "observation", None),
        ("explicit-on", "head", Some(true)),
        ("default-off", "head", None),
        ("explicit-off", "observation", Some(false)),
    ] {
        let mut spec = write(key, None);
        spec.kind = kind.into();
        spec.doc = json!({"marker": key});
        spec.embed = embed;
        members.push(StreamBatchMember::Write(spec));
    }
    let prepared = runtime
        .prepare_stream_batch(&token, members, &registry)
        .await
        .unwrap();
    {
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "one provider request for the complete eligible create set"
        );
        assert_eq!(calls[0].len(), 4);
        assert_eq!(calls[0].iter().collect::<HashSet<_>>().len(), 4);
        for doc in [
            json!({"event": "batch"}),
            json!({"marker": "first"}),
            json!({"marker": "second"}),
            json!({"marker": "explicit-on"}),
        ] {
            let text = serde_json::to_string(&doc).unwrap();
            assert!(
                calls[0].iter().any(|input| input.contains(&text)),
                "missing document {text:?}"
            );
        }
        assert!(calls[0]
            .iter()
            .all(|input| !input.contains("default-off") && !input.contains("explicit-off")));
    }
    assert_eq!(
        vectors(&runtime, &token).await,
        0,
        "preparation must not insert vectors"
    );
    let (values, effects) = run_prepared_stream_batch(
        runtime.sql().as_ref(),
        token.namespace().as_str().into(),
        prepared,
        None,
        vec![],
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(values.len(), 6);
    assert_eq!(values[0]["seq"], 1);
    for value in &values[1..] {
        assert_eq!(value["version"], 1);
    }
    crate::atomic_prepare::apply_post_commit_effects_with_report(&runtime, &token, effects)
        .await
        .unwrap();
    assert_eq!(vectors(&runtime, &token).await, 4);
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn stream_batch_write_tags_and_embedding_transitions_use_canonical_plans() {
    let (runtime, token, registry) = fixture();
    let mut create = write("document", None);
    create.tags = Some(vec!["kept".into()]);
    let first = batch_write(&runtime, &token, &registry, create).await;
    assert_eq!(first["version"], 1);
    assert_eq!(
        vectors(&runtime, &token).await,
        0,
        "head create defaults off"
    );

    let second = batch_write(&runtime, &token, &registry, write("document", Some(1))).await;
    assert_eq!(second["version"], 2);
    let note = runtime
        .get_note_by_key(&token, "document", Some("head"), false)
        .await
        .unwrap();
    assert_eq!(note.properties.as_ref().unwrap()["tags"], json!(["kept"]));
    assert_eq!(vectors(&runtime, &token).await, 0, "omission preserves off");

    let mut on = write("document", Some(2));
    on.tags = Some(vec![]);
    on.embed = Some(true);
    assert_eq!(
        batch_write(&runtime, &token, &registry, on).await["version"],
        3
    );
    let note = runtime
        .get_note_by_key(&token, "document", Some("head"), false)
        .await
        .unwrap();
    assert_eq!(note.properties.as_ref().unwrap()["tags"], json!([]));
    assert_eq!(vectors(&runtime, &token).await, 1);

    assert_eq!(
        batch_write(&runtime, &token, &registry, write("document", Some(3))).await["version"],
        4
    );
    assert_eq!(vectors(&runtime, &token).await, 1, "omission preserves on");
    let mut off = write("document", Some(4));
    off.embed = Some(false);
    assert_eq!(
        batch_write(&runtime, &token, &registry, off).await["version"],
        5
    );
    assert_eq!(
        vectors(&runtime, &token).await,
        0,
        "explicit off purges vector rows"
    );
    let deletes = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(statement(
            "SELECT COUNT(*) FROM ann_write_log WHERE subject_id=?1 AND op='delete'",
            vec![SqlValue::Text(first["id"].as_str().unwrap().into())],
        ))
        .await
        .unwrap();
    assert!(matches!(deletes, Some(SqlValue::Integer(count)) if count > 0));
    let mut ordinary = write("ordinary", None);
    ordinary.kind = "observation".into();
    batch_write(&runtime, &token, &registry, ordinary).await;
    assert_eq!(
        vectors(&runtime, &token).await,
        1,
        "non-head create defaults on"
    );
}

#[tokio::test]
async fn stream_batch_write_missing_and_late_key_conflicts_keep_member_index() {
    let (runtime, token, registry) = fixture();
    let result = runtime
        .stream_batch_atomic(
            &token,
            vec![
                append("missing", None),
                StreamBatchMember::Write(write("missing", Some(1))),
            ],
            None,
            vec![],
            &registry,
        )
        .await
        .unwrap();
    let refusal = result.unwrap_err();
    assert_eq!(refusal.member, 1);
    let error = serde_json::to_value(refusal.error).unwrap();
    assert_eq!(error["kind"], "not_found");
    assert_eq!(error["details"]["reason"], "stream_write_not_found");
    assert_eq!(error["details"]["member"], "1");
    assert_eq!(
        runtime.stream_stat(&token, "missing").await.unwrap()["count"],
        0
    );

    let prepared = runtime
        .prepare_stream_batch(
            &token,
            vec![
                append("collision", None),
                StreamBatchMember::Write(write("held", None)),
            ],
            &registry,
        )
        .await
        .unwrap();
    let holder = batch_write(&runtime, &token, &registry, write("held", None)).await;
    let refusal = run_prepared_stream_batch(
        runtime.sql().as_ref(),
        token.namespace().as_str().into(),
        prepared,
        None,
        vec![],
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    let error = serde_json::to_value(refusal.error).unwrap();
    assert_eq!(refusal.member, 1);
    assert_eq!(error["details"]["reason"], "key_conflict");
    assert_eq!(error["details"]["member"], "1");
    assert_eq!(error["details"]["existing_id"], holder["id"]);
    assert_eq!(
        runtime.stream_stat(&token, "collision").await.unwrap()["count"],
        0
    );
}

#[tokio::test]
async fn stream_batch_successful_write_rolls_back_with_late_append_refusal() {
    let (runtime, token, registry) = fixture();
    let first = batch_write(&runtime, &token, &registry, write("rollback", None)).await;
    let result = runtime
        .stream_batch_atomic(
            &token,
            vec![
                StreamBatchMember::Write(write("rollback", Some(1))),
                append("rollback", Some(99)),
            ],
            None,
            vec![],
            &registry,
        )
        .await
        .unwrap();
    assert_eq!(result.unwrap_err().member, 1);
    let unchanged = runtime
        .get_note_by_key(&token, "rollback", Some("head"), false)
        .await
        .unwrap();
    assert_eq!(unchanged.id.to_string(), first["id"].as_str().unwrap());
    assert_eq!(
        unchanged.version, 1,
        "a write cannot commit in a separate member transaction"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&unchanged.content).unwrap(),
        json!({"revision": 0})
    );
    assert_eq!(
        runtime.stream_stat(&token, "rollback").await.unwrap()["count"],
        0
    );
    let result = runtime
        .stream_batch_per_member(
            &token,
            vec![
                StreamBatchMember::Write(write("rollback", Some(1))),
                append("rollback", Some(99)),
            ],
            &registry,
        )
        .await
        .unwrap();
    assert_eq!(result[0]["version"], 2);
    assert_eq!(result[1]["details"]["reason"], "seq_conflict");
    assert!(result[1]["details"].get("member").is_none());
}

struct AdmissionChange {
    inner: Arc<dyn SqlAccess>,
    changer: Arc<dyn SqlAccess>,
    change: Mutex<Option<SqlStatement>>,
}

#[async_trait]
impl SqlAccess for AdmissionChange {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
        self.inner.writer().await
    }
    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
        let change = self.change.lock().unwrap().take();
        if let Some(change) = change {
            self.changer.writer().await?.execute(change).await?;
        }
        self.inner.atomic_unit(op).await
    }
}

fn file_fixture() -> (
    tempfile::TempDir,
    KhiveRuntime,
    KhiveRuntime,
    NamespaceToken,
    VerbRegistry,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::RuntimeConfig {
        db_path: Some(dir.path().join("stream-batch.db")),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::new(config.clone()).unwrap();
    let peer = KhiveRuntime::new(config).unwrap();
    runtime.install_kind_registry(vec![], vec!["head".into(), "observation".into()]);
    peer.install_kind_registry(vec![], vec!["head".into(), "observation".into()]);
    let token = runtime.authorize(Namespace::local()).unwrap();
    (
        dir,
        runtime,
        peer,
        token,
        VerbRegistryBuilder::new().build().unwrap(),
    )
}

async fn stream_store_snapshot(runtime: &KhiveRuntime) -> Value {
    let mut reader = runtime.sql().reader().await.unwrap();
    let mut snapshot = serde_json::Map::new();
    for (table, order) in [
        ("notes", "id"),
        ("notes_seq", "seq"),
        ("note_streams", "namespace, stream, seq"),
        ("events", "id"),
        ("fts_notes", "rowid"),
        ("fts_notes_rowids", "rowid"),
        ("fts_notes_rowids_state", "key"),
        ("ann_write_log", "rowid"),
        ("sqlite_sequence", "name"),
    ] {
        let rows = reader
            .query_all(statement(
                &format!("SELECT * FROM {table} ORDER BY {order}"),
                vec![],
            ))
            .await
            .unwrap();
        snapshot.insert(table.into(), serde_json::to_value(rows).unwrap());
    }
    Value::Object(snapshot)
}

#[tokio::test]
async fn stream_batch_recreated_key_between_prepare_and_commit_is_version_conflict() {
    for replacement in [None, Some(false), Some(true)] {
        let (_dir, runtime, peer, token, registry) = file_fixture();
        let peer_token = peer.authorize(Namespace::local()).unwrap();
        let first = batch_write(&runtime, &token, &registry, write("target", None)).await;
        let first_id = Uuid::parse_str(first["id"].as_str().unwrap()).unwrap();
        let fired = Arc::new(Mutex::new(Vec::new()));
        let hook_fired = fired.clone();
        runtime.install_note_mutation_hook(Arc::new(move |kind: String, id: Uuid| {
            let fired = hook_fired.clone();
            Box::pin(async move { fired.lock().unwrap().push((kind, id)) })
        }));
        let prepared = runtime
            .prepare_stream_batch(
                &token,
                vec![
                    StreamBatchMember::Write(write("candidate", None)),
                    append("recreated", None),
                    StreamBatchMember::Write(write("target", Some(1))),
                    append("recreated", None),
                ],
                &registry,
            )
            .await
            .unwrap();
        if let Some(hard) = replacement {
            // The old identity disappears after preparation; its replacement has
            // the same version, so comparing only versions cannot detect this race.
            assert!(peer.delete_note(&peer_token, first_id, hard).await.unwrap());
            let mut recreated = write("target", None);
            recreated.doc = json!({"replacement": true});
            let holder = batch_write(&peer, &peer_token, &registry, recreated).await;
            assert_ne!(holder["id"], first["id"]);
            assert_eq!(holder["version"], 1);
        }
        let holder_before = runtime
            .get_note_by_key(&token, "target", Some("head"), false)
            .await
            .unwrap();
        let baseline = stream_store_snapshot(&runtime).await;
        let outcome = run_prepared_stream_batch(
            runtime.sql().as_ref(),
            token.namespace().as_str().into(),
            prepared,
            None,
            vec![],
        )
        .await
        .unwrap();
        if replacement.is_some() {
            let refusal = outcome.expect_err("a recreated holder refuses the stale plan");
            assert_eq!(refusal.member, 2);
            let error = serde_json::to_value(refusal.error).unwrap();
            assert_eq!(error["kind"], "conflict");
            assert_eq!(error["details"]["reason"], "version_conflict");
            assert_eq!(error["details"]["member"], "2");
            assert_eq!(error["details"]["expected_version"], "1");
            assert_eq!(error["details"]["current_version"], "1");
            assert_eq!(stream_store_snapshot(&runtime).await, baseline);
            assert_eq!(
                runtime
                    .get_note_by_key(&token, "target", Some("head"), false)
                    .await
                    .unwrap(),
                holder_before,
            );
            assert_eq!(
                runtime.stream_stat(&token, "recreated").await.unwrap()["count"],
                0
            );
            assert!(fired.lock().unwrap().is_empty());
        } else {
            let (values, effects) = outcome.expect("an unchanged holder commits");
            assert_eq!(values[2]["id"], first["id"]);
            assert_eq!(values[2]["version"], 2);
            assert_eq!(values[1]["seq"], 1);
            assert_eq!(values[3]["seq"], 2);
            assert!(!effects.as_slice().is_empty());
            assert!(fired.lock().unwrap().is_empty());
            crate::atomic_prepare::apply_post_commit_effects_with_report(&runtime, &token, effects)
                .await
                .unwrap();
            assert!(fired.lock().unwrap().iter().any(|(_, id)| *id == first_id));
            assert_eq!(
                runtime.stream_stat(&token, "recreated").await.unwrap()["count"],
                2
            );
        }
    }
}

struct OutcomeAccess {
    inner: Arc<dyn SqlAccess>,
    invoke: bool,
    expect_success: bool,
    terminate: bool,
    state: WriterTaskRequestState,
    callback: Arc<Mutex<Option<bool>>>,
}

#[async_trait]
impl SqlAccess for OutcomeAccess {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
        self.inner.writer().await
    }
    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
        if self.invoke {
            let callback = self.callback.clone();
            let traced: AtomicUnitOp = Box::new(move |writer| {
                Box::pin(async move {
                    let outcome = op(writer).await;
                    *callback.lock().unwrap() = Some(outcome.is_ok());
                    outcome
                })
            });
            let outcome = self.inner.atomic_unit(traced).await;
            assert_eq!(outcome.is_ok(), self.expect_success);
        }
        if self.terminate {
            Err(StorageError::WriterTaskTerminated {
                request_state: self.state,
            })
        } else {
            Err(StorageError::WriterTaskRequestFailed {
                request_state: self.state,
                source: Box::new(StorageError::Pool {
                    operation: "stream_test_ack".into(),
                    message: "injected acknowledgment failure".into(),
                }),
            })
        }
    }
}

#[tokio::test]
async fn stream_batch_recovers_failure_slot_only_after_confirmed_rollback() {
    use WriterTaskRequestState::{NotStarted, SideEffectsUnknown, TransactionRolledBack};
    for (invoke, terminate, state) in [
        (true, false, TransactionRolledBack),
        (true, false, SideEffectsUnknown),
        (true, true, SideEffectsUnknown),
        (true, true, TransactionRolledBack),
        (false, false, NotStarted),
        (false, false, TransactionRolledBack),
    ] {
        let (_dir, runtime, _peer, token, registry) = file_fixture();
        let prepared = runtime
            .prepare_stream_batch(
                &token,
                vec![
                    StreamBatchMember::Write(write("candidate", None)),
                    append("refused", Some(9)),
                ],
                &registry,
            )
            .await
            .unwrap();
        let baseline = stream_store_snapshot(&runtime).await;
        let callback = Arc::new(Mutex::new(None));
        let access = OutcomeAccess {
            inner: runtime.sql(),
            invoke,
            expect_success: false,
            terminate,
            state,
            callback: callback.clone(),
        };
        let outcome = run_prepared_stream_batch(
            &access,
            token.namespace().as_str().into(),
            prepared,
            None,
            vec![],
        )
        .await;
        assert_eq!(*callback.lock().unwrap(), invoke.then_some(false));
        if invoke && !terminate && state == TransactionRolledBack {
            let refusal = outcome
                .unwrap()
                .expect_err("recover the recorded member failure");
            assert_eq!(refusal.member, 1);
            let error = serde_json::to_value(refusal.error).unwrap();
            assert_eq!(error["details"]["reason"], "seq_conflict");
            assert_eq!(error["details"]["member"], "1");
            assert_eq!(error["details"]["next_seq"], "1");
        } else {
            match outcome.expect_err("preserve the outer storage failure") {
                RuntimeError::Storage(StorageError::WriterTaskTerminated { request_state }) => {
                    assert!(terminate);
                    assert_eq!(request_state, state);
                }
                RuntimeError::Storage(StorageError::WriterTaskRequestFailed {
                    request_state,
                    source,
                }) => {
                    assert!(!terminate);
                    assert_eq!(request_state, state);
                    assert!(
                        matches!(*source, StorageError::Pool { operation, .. } if operation == "stream_test_ack")
                    );
                }
                error => panic!("unexpected storage outcome: {error:?}"),
            }
        }
        assert_eq!(stream_store_snapshot(&runtime).await, baseline);
    }
}

#[tokio::test]
async fn stream_batch_commit_ack_failure_returns_no_executable_effects() {
    let (_dir, runtime, _peer, token, registry) = file_fixture();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let hook_fired = fired.clone();
    runtime.install_note_mutation_hook(Arc::new(move |kind: String, id: Uuid| {
        let fired = hook_fired.clone();
        Box::pin(async move { fired.lock().unwrap().push((kind, id)) })
    }));
    let prepared = runtime
        .prepare_stream_batch(
            &token,
            vec![
                StreamBatchMember::Write(write("acknowledgment", None)),
                append("acknowledgment", None),
            ],
            &registry,
        )
        .await
        .unwrap();
    let callback = Arc::new(Mutex::new(None));
    let access = OutcomeAccess {
        inner: runtime.sql(),
        invoke: true,
        expect_success: true,
        terminate: true,
        state: WriterTaskRequestState::SideEffectsUnknown,
        callback: callback.clone(),
    };
    let outcome = run_prepared_stream_batch(
        &access,
        token.namespace().as_str().into(),
        prepared,
        None,
        vec![],
    )
    .await;
    assert!(matches!(
        outcome,
        Err(RuntimeError::Storage(StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
        }))
    ));
    assert_eq!(*callback.lock().unwrap(), Some(true));
    assert!(fired.lock().unwrap().is_empty());
    // The injected error loses the acknowledgment, not the committed writes.
    assert_eq!(
        runtime.stream_stat(&token, "acknowledgment").await.unwrap()["count"],
        1
    );
    assert_eq!(
        runtime
            .get_note_by_key(&token, "acknowledgment", Some("head"), false)
            .await
            .unwrap()
            .version,
        1
    );
}

#[tokio::test]
async fn stream_batch_fence_rechecks_after_writer_admission() {
    let (_dir, runtime, peer, token, registry) = file_fixture();
    let fence = batch_write(&runtime, &token, &registry, write("fence", None)).await;
    let prepared = runtime
        .prepare_stream_batch(&token, vec![append("fenced", None)], &registry)
        .await
        .unwrap();
    let access = AdmissionChange {
        inner: runtime.sql().clone(),
        changer: peer.sql(),
        change: Mutex::new(Some(statement(
            "UPDATE notes SET content=?1 WHERE id=?2",
            vec![
                SqlValue::Text("{\"revision\":1}".into()),
                SqlValue::Text(fence["id"].as_str().unwrap().into()),
            ],
        ))),
    };
    let result = run_prepared_stream_batch(
        &access,
        token.namespace().as_str().into(),
        prepared,
        Some(NoteFence {
            key: "fence".into(),
            kind: "head".into(),
            expected_version: 1,
        }),
        vec![],
    )
    .await;
    let Err(RuntimeError::Khive(error)) = result else {
        panic!("late fence change must refuse")
    };
    let details = serde_json::to_value(error.details().unwrap()).unwrap();
    assert_eq!(details["reason"], "fence_conflict");
    assert_eq!(details["current_version"], "2");
    assert_eq!(
        runtime.stream_stat(&token, "fenced").await.unwrap()["count"],
        0
    );
}

#[tokio::test]
async fn stream_batch_observation_rechecks_cross_connection_change_at_admission() {
    let (_dir, runtime, peer, token, registry) = file_fixture();
    let observed = batch_write(&runtime, &token, &registry, write("observed", None)).await;
    let prepared = runtime
        .prepare_stream_batch(&token, vec![append("observed", None)], &registry)
        .await
        .unwrap();
    let access = AdmissionChange {
        inner: runtime.sql(),
        changer: peer.sql(),
        change: Mutex::new(Some(statement(
            "UPDATE notes SET content=?1 WHERE id=?2",
            vec![
                SqlValue::Text("{\"revision\":1}".into()),
                SqlValue::Text(observed["id"].as_str().unwrap().into()),
            ],
        ))),
    };
    let result = run_prepared_stream_batch(
        &access,
        token.namespace().as_str().into(),
        prepared,
        None,
        vec![StreamObservation {
            key: "observed".into(),
            kind: "head".into(),
            version: Some(1),
            live_until: None,
        }],
    )
    .await;
    let Err(RuntimeError::Khive(error)) = result else {
        panic!("late observed change must refuse")
    };
    let details = serde_json::to_value(error.details().unwrap()).unwrap();
    assert_eq!(details["reason"], "version_conflict");
    assert_eq!(details["index"], "0");
    assert_eq!(details["current_version"], "2");
    assert_eq!(
        runtime.stream_stat(&token, "observed").await.unwrap()["count"],
        0
    );
}

#[tokio::test]
async fn stream_batch_append_member_fence_rechecks_cross_connection_at_admission() {
    let (_dir, runtime, peer, token, registry) = file_fixture();
    batch_write(&runtime, &token, &registry, write("stable", None)).await;
    let renewed = batch_write(&runtime, &token, &registry, write("renewed", None)).await;
    let prepared = runtime
        .prepare_stream_batch(
            &token,
            vec![
                append("member-race", None),
                fenced_append(
                    "member-race",
                    vec![
                        NoteFence {
                            key: "stable".into(),
                            kind: "head".into(),
                            expected_version: 1,
                        },
                        NoteFence {
                            key: "renewed".into(),
                            kind: "head".into(),
                            expected_version: 1,
                        },
                    ],
                ),
            ],
            &registry,
        )
        .await
        .unwrap();
    let access = AdmissionChange {
        inner: runtime.sql(),
        changer: peer.sql(),
        change: Mutex::new(Some(statement(
            "UPDATE notes SET content=?1 WHERE id=?2",
            vec![
                SqlValue::Text("{\"revision\":1}".into()),
                SqlValue::Text(renewed["id"].as_str().unwrap().into()),
            ],
        ))),
    };
    let result = run_prepared_stream_batch(
        &access,
        token.namespace().as_str().into(),
        prepared,
        None,
        vec![],
    )
    .await
    .unwrap();
    let Err(refusal) = result else {
        panic!("late append-member fence change must refuse")
    };
    assert_eq!(refusal.member, 1);
    let details = serde_json::to_value(refusal.error.details().unwrap()).unwrap();
    assert_eq!(details["reason"], "fence_conflict");
    assert_eq!(details["member"], "1");
    assert_eq!(details["index"], "1");
    assert_eq!(details["current_version"], "2");
    assert_eq!(
        runtime.stream_stat(&token, "member-race").await.unwrap()["count"],
        0
    );
    assert_eq!(
        runtime
            .get_note_by_key(&token, "renewed", Some("head"), false)
            .await
            .unwrap()
            .version,
        2
    );
}

// This writer records statements, not rolled-back row counts: moving an
// observation after an INSERT must be visible even if the unit later rolls back.
#[derive(Clone)]
struct TraceAccess(Arc<Mutex<Vec<SqlStatement>>>);

#[async_trait]
impl SqlReader for TraceAccess {
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
        assert_eq!(statement.label.as_deref(), Some("stream-batch-write-time"));
        self.0.lock().unwrap().push(statement);
        Ok(Some(SqlRow {
            columns: vec![
                khive_storage::SqlColumn {
                    name: "version".into(),
                    value: SqlValue::Integer(1),
                },
                khive_storage::SqlColumn {
                    name: "updated_at".into(),
                    value: SqlValue::Integer(1),
                },
            ],
        }))
    }
    async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        unreachable!()
    }
    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        if statement.label.as_deref() == Some("stream-batch-live-until") {
            let expired =
                matches!(statement.params.last(), Some(SqlValue::Text(key)) if key == "expired");
            let content = json!({"expires_at": if expired {"1970-01-01T00:00:00.000001Z"} else {"1970-01-01T00:00:00.000002Z"}}).to_string();
            self.0.lock().unwrap().push(statement);
            return Ok(Some(SqlValue::Text(content)));
        }
        let value = if statement.sql.contains("MAX(seq)") {
            0
        } else {
            1
        };
        self.0.lock().unwrap().push(statement);
        Ok(Some(SqlValue::Integer(value)))
    }
    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        unreachable!()
    }
}

#[async_trait]
impl SqlWriter for TraceAccess {
    async fn execute(&mut self, statement: SqlStatement) -> StorageResult<u64> {
        self.0.lock().unwrap().push(statement);
        Ok(1)
    }
    async fn execute_batch(&mut self, _: Vec<SqlStatement>) -> StorageResult<u64> {
        unreachable!()
    }
    async fn execute_script(&mut self, _: String) -> StorageResult<()> {
        unreachable!()
    }
}

#[async_trait]
impl SqlAccess for TraceAccess {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        Ok(Box::new(self.clone()))
    }
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
        Ok(Box::new(self.clone()))
    }
    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
        self.0.lock().unwrap().push(statement("BEGIN", vec![]));
        let result = op(&mut self.clone()).await;
        self.0.lock().unwrap().push(statement(
            if result.is_ok() { "COMMIT" } else { "ROLLBACK" },
            vec![],
        ));
        result
    }
}

fn interleaved_appends() -> Vec<StreamBatchMember> {
    ["a", "b", "a", "a", "b"]
        .into_iter()
        .enumerate()
        .map(|(index, stream)| {
            let StreamBatchMember::Append(mut spec) = append(stream, None) else {
                unreachable!()
            };
            spec.record = json!({"position": index});
            StreamBatchMember::Append(spec)
        })
        .collect()
}

#[tokio::test]
async fn stream_batch_reads_each_stream_head_once_and_allocates_in_order() {
    let (runtime, token, registry) = fixture();
    let prepared = runtime
        .prepare_stream_batch(&token, interleaved_appends(), &registry)
        .await
        .unwrap();
    let trace = TraceAccess(Arc::new(Mutex::new(vec![])));
    let (values, _) = run_prepared_stream_batch(
        &trace,
        token.namespace().as_str().into(),
        prepared,
        None,
        vec![],
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(values.len(), 5);
    let trace = trace.0.lock().unwrap();
    let heads: Vec<_> = trace
        .iter()
        .filter(|statement| statement.sql.contains("MAX(seq)"))
        .collect();
    assert_eq!(heads.len(), 2, "one head query per distinct stream");
    for (head, stream) in heads.iter().zip(["a", "b"]) {
        assert!(matches!(&head.params[0], SqlValue::Text(ns) if ns == token.namespace().as_str()));
        assert!(matches!(&head.params[1], SqlValue::Text(name) if name == stream));
    }
    let inserts: Vec<_> = trace
        .iter()
        .filter(|statement| statement.sql.starts_with("INSERT INTO note_streams"))
        .collect();
    assert_eq!(inserts.len(), 5);
    for ((insert, value), (stream, seq)) in
        inserts
            .iter()
            .zip(&values)
            .zip([("a", 1), ("b", 1), ("a", 2), ("a", 3), ("b", 2)])
    {
        assert!(matches!(&insert.params[1], SqlValue::Text(name) if name == stream));
        assert!(matches!(&insert.params[2], SqlValue::Integer(actual) if *actual == seq));
        assert!(
            matches!(&insert.params[3], SqlValue::Text(id) if Some(id.as_str()) == value["id"].as_str())
        );
        assert_eq!(value["seq"], seq);
    }
    assert_eq!(trace.first().unwrap().sql, "BEGIN");
    assert_eq!(trace.last().unwrap().sql, "COMMIT");
}

#[tokio::test]
async fn stream_batch_head_allocation_persists_order_and_refreshes_next_transaction() {
    let (_dir, runtime, _peer, token, registry) = file_fixture();
    for expected in [[1, 1, 2, 3, 2], [4, 3, 5, 6, 4]] {
        let values = runtime
            .stream_batch_atomic(&token, interleaved_appends(), None, vec![], &registry)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(values.len(), 5);
        for (value, seq) in values.iter().zip(expected) {
            assert_eq!(value["seq"], seq);
        }
    }
    for (stream, positions) in [("a", vec![0, 2, 3, 0, 2, 3]), ("b", vec![1, 4, 1, 4])] {
        let page = runtime.stream_read(&token, stream, 0, 100).await.unwrap();
        assert_eq!(page["head_seq"], positions.len());
        let entries = page["entries"].as_array().unwrap();
        assert_eq!(entries.len(), positions.len());
        for (index, (entry, position)) in entries.iter().zip(positions).enumerate() {
            assert_eq!(entry["seq"], index + 1);
            assert_eq!(entry["record"]["position"], position);
        }
    }
    let baseline = stream_store_snapshot(&runtime).await;
    let refusal = runtime
        .stream_batch_atomic(
            &token,
            vec![
                append("a", Some(7)),
                append("b", Some(5)),
                append("a", Some(7)),
            ],
            None,
            vec![],
            &registry,
        )
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(refusal.member, 2);
    let error = serde_json::to_value(refusal.error).unwrap();
    assert_eq!(error["details"]["reason"], "seq_conflict");
    assert_eq!(error["details"]["member"], "2");
    assert_eq!(error["details"]["expected_seq"], "7");
    assert_eq!(error["details"]["next_seq"], "8");
    assert_eq!(stream_store_snapshot(&runtime).await, baseline);
}

#[tokio::test]
async fn stream_batch_observed_statement_trace_precedes_first_member_insert() {
    let (runtime, token, registry) = fixture();
    for stale in [false, true] {
        let prepared = runtime
            .prepare_stream_batch(&token, vec![append("trace", None)], &registry)
            .await
            .unwrap();
        let trace = TraceAccess(Arc::new(Mutex::new(vec![])));
        let observed = vec![
            StreamObservation {
                key: "first".into(),
                kind: "head".into(),
                version: Some(1),
                live_until: None,
            },
            StreamObservation {
                key: "second".into(),
                kind: "head".into(),
                version: Some(if stale { 2 } else { 1 }),
                live_until: None,
            },
        ];
        let result = run_prepared_stream_batch(
            &trace,
            token.namespace().as_str().into(),
            prepared,
            None,
            observed,
        )
        .await;
        assert_eq!(result.is_ok(), !stale);
        let trace = trace.0.lock().unwrap();
        let observations: Vec<_> = trace
            .iter()
            .enumerate()
            .filter(|(_, s)| s.label.as_deref() == Some("stream-batch-observed"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            observations.len(),
            2,
            "each observation checked exactly once"
        );
        let first_insert = trace.iter().position(|s| s.sql.starts_with("INSERT"));
        if stale {
            assert!(
                first_insert.is_none(),
                "stale observations must precede every member INSERT, even rolled-back inserts"
            );
        } else {
            assert!(
                observations[1] < first_insert.unwrap(),
                "all observations precede first member INSERT"
            );
        }
        assert_eq!(trace[0].sql, "BEGIN");
    }
}

#[tokio::test]
async fn stream_batch_append_member_fences_precede_every_member_insert() {
    let (runtime, token, registry) = fixture();
    for stale in [false, true] {
        let prepared = runtime
            .prepare_stream_batch(
                &token,
                vec![
                    append("member-trace", None),
                    fenced_append(
                        "member-trace",
                        vec![
                            NoteFence {
                                key: "first".into(),
                                kind: "head".into(),
                                expected_version: 1,
                            },
                            NoteFence {
                                key: "second".into(),
                                kind: "head".into(),
                                expected_version: if stale { 2 } else { 1 },
                            },
                        ],
                    ),
                ],
                &registry,
            )
            .await
            .unwrap();
        let trace = TraceAccess(Arc::new(Mutex::new(vec![])));
        let result = run_prepared_stream_batch(
            &trace,
            token.namespace().as_str().into(),
            prepared,
            None,
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(result.is_ok(), !stale);
        let trace = trace.0.lock().unwrap();
        let checks: Vec<_> = trace
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.label.as_deref() == Some("note-write-guard")
                    && s.sql.starts_with("SELECT version")
            })
            .collect();
        assert_eq!(checks.len(), 2, "each append-member fence checked once");
        assert!(matches!(&checks[0].1.params[2], SqlValue::Text(key) if key == "first"));
        assert!(matches!(&checks[1].1.params[2], SqlValue::Text(key) if key == "second"));
        let first_insert = trace.iter().position(|s| s.sql.starts_with("INSERT"));
        if stale {
            assert!(
                first_insert.is_none(),
                "stale append-member fences precede every INSERT, even rolled-back writes"
            );
        } else {
            assert!(
                checks[1].0 < first_insert.unwrap(),
                "all append-member fences precede first member INSERT"
            );
        }
        assert_eq!(trace[0].sql, "BEGIN");
    }
}

#[derive(Debug)]
struct NormalizeHook(Arc<Mutex<Vec<Uuid>>>);

#[async_trait]
impl KindHook for NormalizeHook {
    async fn prepare_create(&self, _: &KhiveRuntime, args: &mut Value) -> RuntimeResult<()> {
        if args["key"] == "internal" {
            return Err(KhiveError::internal("hook infrastructure failed").into());
        }
        if args["key"] == "embed-off" {
            args["embed"] = json!(false);
        } else if args["key"] == "embed-malformed" {
            args["embed"] = json!("false");
        }
        args["content"] = json!("{\"normalized\":\"create\"}");
        args["properties"] = json!({"normalized": "create"});
        Ok(())
    }
    async fn after_create(&self, _: &KhiveRuntime, id: Uuid, _: &Value) -> RuntimeResult<()> {
        self.0.lock().unwrap().push(id);
        Ok(())
    }
    async fn prepare_note_update(
        &self,
        _: &KhiveRuntime,
        _: &NamespaceToken,
        _: &Note,
        args: &mut Value,
    ) -> RuntimeResult<()> {
        args["content"] = json!("{\"normalized\":\"update\"}");
        args["properties"] = json!({"normalized": "update"});
        Ok(())
    }
}

struct TestPack(Arc<NormalizeHook>);
impl Pack for TestPack {
    const NAME: &'static str = "stream-test";
    const NOTE_KINDS: &'static [&'static str] = &["normalized"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for TestPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        (kind == "normalized").then(|| self.0.clone() as Arc<dyn KindHook>)
    }
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> RuntimeResult<Value> {
        unreachable!()
    }
}

#[tokio::test]
async fn stream_batch_kind_hooks_normalize_both_writes_and_run_only_after_commit() {
    let (runtime, token, _) = fixture();
    let creates = Arc::new(Mutex::new(vec![]));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(TestPack(Arc::new(NormalizeHook(creates.clone()))));
    let registry = builder.build().unwrap();
    let mut create = write("hook", None);
    create.kind = "normalized".into();
    create.embed = Some(false);
    let first = batch_write(&runtime, &token, &registry, create).await;
    let note = runtime
        .get_note_by_key(&token, "hook", Some("normalized"), false)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&note.content).unwrap(),
        json!({"normalized": "create"})
    );
    assert_eq!(note.properties.as_ref().unwrap()["normalized"], "create");
    assert_eq!(creates.lock().unwrap().as_slice(), &[note.id]);
    let mut update = write("hook", Some(1));
    update.kind = "normalized".into();
    assert_eq!(
        batch_write(&runtime, &token, &registry, update).await["version"],
        2
    );
    let note = runtime
        .get_note_by_key(&token, "hook", Some("normalized"), false)
        .await
        .unwrap();
    assert_eq!(note.id.to_string(), first["id"].as_str().unwrap());
    assert_eq!(
        serde_json::from_str::<Value>(&note.content).unwrap(),
        json!({"normalized": "update"})
    );
    assert_eq!(note.properties.as_ref().unwrap()["normalized"], "update");
    let mut aborted = write("aborted", None);
    aborted.kind = "normalized".into();
    aborted.embed = Some(false);
    let result = runtime
        .stream_batch_atomic(
            &token,
            vec![
                StreamBatchMember::Write(aborted),
                append("aborted", Some(99)),
            ],
            None,
            vec![],
            &registry,
        )
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        creates.lock().unwrap().len(),
        1,
        "rollback must not run create callbacks"
    );
}

#[tokio::test]
async fn stream_batch_internal_hook_errors_are_outer_failures_before_writes() {
    let (runtime, token, _) = fixture();
    let creates = Arc::new(Mutex::new(vec![]));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(TestPack(Arc::new(NormalizeHook(creates.clone()))));
    let registry = builder.build().unwrap();
    for atomic in [true, false] {
        let mut spec = write("internal", None);
        spec.kind = "normalized".into();
        let members = vec![append("internal", None), StreamBatchMember::Write(spec)];
        let error = if atomic {
            runtime
                .stream_batch_atomic(&token, members, None, vec![], &registry)
                .await
                .err()
                .unwrap()
        } else {
            runtime
                .stream_batch_per_member(&token, members, &registry)
                .await
                .err()
                .unwrap()
        };
        assert!(
            matches!(error, RuntimeError::Khive(ref error) if error.kind() == khive_types::ErrorKind::Internal)
        );
        assert_eq!(
            runtime.stream_stat(&token, "internal").await.unwrap()["count"],
            0
        );
    }
    assert!(creates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stream_batch_create_hook_embed_false_overrides_explicit_true() {
    let (runtime, token, _) = fixture();
    let creates = Arc::new(Mutex::new(vec![]));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(TestPack(Arc::new(NormalizeHook(creates.clone()))));
    let registry = builder.build().unwrap();
    let mut spec = write("embed-off", None);
    spec.kind = "normalized".into();
    spec.embed = Some(true);
    let result = batch_write(&runtime, &token, &registry, spec).await;
    let note = runtime
        .get_note_by_key(&token, "embed-off", Some("normalized"), false)
        .await
        .unwrap();
    assert_eq!(note.id.to_string(), result["id"].as_str().unwrap());
    assert_eq!(note.version, 1);
    assert_eq!(
        vectors(&runtime, &token).await,
        0,
        "post-hook embed=false overrides caller embed=true"
    );
    assert_eq!(creates.lock().unwrap().as_slice(), &[note.id]);
}

#[tokio::test]
async fn stream_batch_create_hook_malformed_embed_refuses_before_any_write() {
    let (runtime, token, _) = fixture();
    let creates = Arc::new(Mutex::new(vec![]));
    let mut builder = VerbRegistryBuilder::new();
    builder.register(TestPack(Arc::new(NormalizeHook(creates.clone()))));
    let registry = builder.build().unwrap();
    for atomic in [true, false] {
        let mut spec = write("embed-malformed", None);
        spec.kind = "normalized".into();
        spec.embed = Some(true);
        let members = vec![
            append("embed-malformed", None),
            StreamBatchMember::Write(spec),
        ];
        let error = if atomic {
            runtime
                .stream_batch_atomic(&token, members, None, vec![], &registry)
                .await
                .err()
                .unwrap()
        } else {
            runtime
                .stream_batch_per_member(&token, members, &registry)
                .await
                .err()
                .unwrap()
        };
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref message)
            if message.starts_with("stream write fields:") && message.contains("boolean")),
            "{error:?}"
        );
        assert_eq!(
            runtime
                .stream_stat(&token, "embed-malformed")
                .await
                .unwrap()["count"],
            0
        );
        assert!(
            matches!(runtime.get_note_by_key(&token, "embed-malformed", Some("normalized"), false).await,
            Err(RuntimeError::Khive(error)) if error.kind() == khive_types::ErrorKind::NotFound)
        );
        assert_eq!(vectors(&runtime, &token).await, 0);
    }
    assert!(creates.lock().unwrap().is_empty());
}

#[path = "streams_expiry_tests.rs"]
mod expiry_tests;
