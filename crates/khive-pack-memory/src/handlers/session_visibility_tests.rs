//! ADR-144 session visibility through the public memory verb dispatch surface.

use std::sync::Arc;

use async_trait::async_trait;
use khive_db::StorageBackend;
use khive_pack_kg::KgPack;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, Namespace, RequestIdentity, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::ErrorKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use serial_test::serial;

use crate::ann::{self, AnnKey, SharedAnn};
use crate::test_support::HashVecProvider;
use crate::MemoryPack;

const MODEL: &str = "adr144-session-visibility-test-model";
const FAILING_MODEL: &str = "adr144-session-failing-test-model";
const CONTENT: &str = "quartz heron lantern session visibility marker";

struct FailingEmbedService;

#[async_trait]
impl EmbeddingService for FailingEmbedService {
    async fn embed(
        &self,
        _texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::ModelNotLoaded(
            "simulated session embedding outage".to_string(),
        ))
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "session-failing-embed"
    }
}

struct FailingEmbedProvider;

#[async_trait]
impl EmbedderProvider for FailingEmbedProvider {
    fn name(&self) -> &str {
        FAILING_MODEL
    }

    fn dimensions(&self) -> usize {
        8
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(FailingEmbedService))
    }
}

fn registry(rt: &KhiveRuntime) -> VerbRegistry {
    registry_with_ann(rt).0
}

fn registry_with_ann(rt: &KhiveRuntime) -> (VerbRegistry, SharedAnn) {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let memory = MemoryPack::new(rt.clone());
    let ann = memory.ann_for_test();
    builder.register(memory);
    (builder.build().expect("registry"), ann)
}

fn vector_runtime() -> KhiveRuntime {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 16,
    });
    rt
}

fn two_vector_runtimes() -> (KhiveRuntime, KhiveRuntime) {
    let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
    backend.prepare_core_schema().expect("core schema");
    let reader = KhiveRuntime::from_backend(backend.clone(), RuntimeConfig::no_embeddings())
        .with_ann_fresh_tail_enabled(false);
    let writer = KhiveRuntime::from_backend(backend, RuntimeConfig::no_embeddings());
    for runtime in [&reader, &writer] {
        runtime.register_embedder(HashVecProvider {
            model_name: MODEL.to_owned(),
            dims: 16,
        });
    }
    (reader, writer)
}

async fn remember(registry: &VerbRegistry, content: &str) -> Value {
    registry
        .dispatch(
            "memory.remember",
            json!({"content": content, "memory_type": "semantic"}),
        )
        .await
        .expect("memory.remember")
}

fn session_request(content: &str, visibility_token: Value) -> Value {
    json!({
        "query": content,
        "limit": 50,
        "score_floor": 0.0,
        "consistency": "session",
        "visibility_token": visibility_token,
        "timeout_ms": 0,
    })
}

fn contains_id(result: &Value, id: &Value) -> bool {
    result
        .as_array()
        .expect("memory.recall returns an array")
        .iter()
        .any(|hit| &hit["id"] == id)
}

fn assert_freshness_unmet(error: RuntimeError, model: &str) {
    let RuntimeError::Khive(domain) = error.refusal_source() else {
        panic!("expected typed freshness_unmet, got {error:?}");
    };
    assert_eq!(domain.kind(), ErrorKind::Unavailable);
    assert_eq!(
        domain.details().and_then(|details| details.get("reason")),
        Some("freshness_unmet")
    );
    assert_eq!(
        domain
            .details()
            .and_then(|details| details.get("failed_models")),
        Some(model)
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn immediate_remember_then_session_recall_returns_the_new_memory() {
    let rt = vector_runtime();
    let registry = registry(&rt);
    let remembered = remember(&registry, CONTENT).await;
    let visibility_token = remembered["visibility_token"].clone();
    assert_eq!(visibility_token["fences"][0]["model"], MODEL);
    assert!(visibility_token["fences"][0]["ann_write_log_seq"]
        .as_u64()
        .is_some_and(|seq| seq > 0));

    let recalled = registry
        .dispatch("memory.recall", session_request(CONTENT, visibility_token))
        .await
        .expect("the immediately preceding write must be visible to session recall");
    assert!(
        contains_id(&recalled, &remembered["id"]),
        "session recall omitted the matching memory: {recalled:?}"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn interleaved_compaction_keeps_session_recall_on_the_same_snapshot() {
    let (reader, writer) = two_vector_runtimes();
    let (reader_registry, reader_ann) = registry_with_ann(&reader);
    let (writer_registry, writer_ann) = registry_with_ann(&writer);
    let reader_token = reader.authorize(Namespace::local()).expect("reader token");
    let writer_token = writer.authorize(Namespace::local()).expect("writer token");
    let key = AnnKey::from_token(MODEL);

    remember(&writer_registry, "quartz heron old segment seed").await;
    ann::ensure_ann_for_model(&reader, &reader_token, &reader_ann, MODEL)
        .await
        .expect("reader serves the old segment");
    let old_watermark = ann::bridge_applied_seq(&reader_ann, &key)
        .await
        .expect("old reader segment watermark");

    let remembered = remember(&writer_registry, CONTENT).await;
    let receipt = remembered["visibility_token"].clone();
    let sequence = receipt["fences"][0]["ann_write_log_seq"]
        .as_u64()
        .expect("write fence");
    assert!(
        old_watermark < sequence,
        "reader segment predates the write"
    );
    ann::ensure_ann_for_model(&writer, &writer_token, &writer_ann, MODEL)
        .await
        .expect("writer publishes a segment covering the write");
    let served_watermark = ann::bridge_applied_seq(&writer_ann, &key)
        .await
        .expect("published segment watermark");
    assert!(served_watermark >= sequence);
    ann::compact_log_for_test(&writer, MODEL)
        .await
        .expect("compact the segment-covered log row");
    let remaining = writer
        .sql()
        .reader()
        .await
        .expect("sql reader")
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log WHERE seq = ?1".into(),
            params: vec![SqlValue::Integer(i64::try_from(sequence).unwrap())],
            label: Some("session-visibility-compacted-fence".into()),
        })
        .await
        .expect("count compacted row");
    assert!(matches!(remaining, Some(SqlValue::Integer(0))));

    let eventual = reader_registry
        .dispatch(
            "memory.recall",
            json!({
                "query": CONTENT,
                "fusion_strategy": "vector_only",
                "limit": 50,
                "score_floor": 0.0,
            }),
        )
        .await
        .expect("stale reader's ordinary route remains available");
    assert!(
        !contains_id(&eventual, &remembered["id"]),
        "fixture must expose the preflight-then-ordinary-recall gap"
    );

    let mut request = session_request(CONTENT, receipt);
    request["fusion_strategy"] = json!("vector_only");
    let recalled = reader_registry
        .dispatch("memory.recall", request)
        .await
        .expect("the exact candidate read must prove the compacted fence");
    assert!(
        contains_id(&recalled, &remembered["id"]),
        "session recall omitted the compacted write: {recalled:?}"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn bound_actor_episodic_receipt_proves_in_its_visible_namespace() {
    let rt = vector_runtime();
    let registry = registry(&rt);
    let identity = || RequestIdentity {
        namespace: "local".to_string(),
        actor_id: Some("lambda:session-probe".to_string()),
        visible_namespaces: vec![],
        ..Default::default()
    };
    let content = "quartz heron actor episodic session marker";
    let remembered = registry
        .dispatch_with_identity(
            "memory.remember",
            json!({"content": content, "memory_type": "episodic"}),
            Some(identity()),
        )
        .await
        .expect("episodic actor remember");
    let receipt = remembered["visibility_token"].clone();
    assert_ne!(receipt["namespace"], "local");
    assert_eq!(receipt["fences"][0]["model"], MODEL);
    let recalled = registry
        .dispatch_with_identity(
            "memory.recall",
            session_request(content, receipt.clone()),
            Some(identity()),
        )
        .await
        .expect("actor-visible receipt namespace must prove on the exact scan");
    assert!(contains_id(&recalled, &remembered["id"]));

    let mut local_only = session_request(content, receipt);
    local_only["namespace"] = json!("local");
    let error = registry
        .dispatch_with_identity("memory.recall", local_only, Some(identity()))
        .await
        .expect_err("explicit local read scope cannot consume an actor receipt");
    assert!(matches!(
        error.refusal_source(),
        RuntimeError::InvalidInput(_)
    ));
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn future_sequence_refuses_with_typed_freshness_unmet_and_model() {
    let rt = vector_runtime();
    let registry = registry(&rt);
    let remembered = remember(&registry, CONTENT).await;
    let mut visibility_token = remembered["visibility_token"].clone();
    let issued_seq = visibility_token["fences"][0]["ann_write_log_seq"]
        .as_u64()
        .expect("issued sequence");
    visibility_token["fences"][0]["ann_write_log_seq"] = json!(issued_seq
        .checked_add(1_000_000)
        .expect("fixture sequence leaves headroom"));

    let (result, probes, exact_statements) = ann::count_session_statements(
        registry.dispatch("memory.recall", session_request(CONTENT, visibility_token)),
    )
    .await;
    let error = result.expect_err("an unobserved future sequence cannot prove session visibility");
    assert_freshness_unmet(error, MODEL);
    assert_eq!(probes, 1, "zero wait performs exactly one fence check");
    assert_eq!(
        exact_statements, 0,
        "an unobserved fence must not run memory_session_exact_snapshot KNN"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn waiting_on_a_future_fence_polls_proof_without_repeating_knn() {
    let rt = vector_runtime();
    let registry = registry(&rt);
    let remembered = remember(&registry, CONTENT).await;
    let mut visibility_token = remembered["visibility_token"].clone();
    let issued_seq = visibility_token["fences"][0]["ann_write_log_seq"]
        .as_u64()
        .expect("issued sequence");
    visibility_token["fences"][0]["ann_write_log_seq"] = json!(issued_seq + 1_000_000);
    let mut request = session_request(CONTENT, visibility_token);
    request["timeout_ms"] = json!(120);

    let started = std::time::Instant::now();
    let (result, probes, exact_statements) =
        ann::count_session_statements(registry.dispatch("memory.recall", request)).await;
    assert_freshness_unmet(result.expect_err("future fence cannot arrive"), MODEL);
    assert!((1..=5).contains(&probes), "40 ms probe cadence: {probes}");
    assert_eq!(
        exact_statements, 0,
        "polling an unmet fence must not dispatch memory_session_exact_snapshot"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(750),
        "retry work must stop near the caller's 120 ms wait window"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn missing_original_log_proof_refuses_even_when_text_recall_finds_memory() {
    let rt = vector_runtime().with_ann_fresh_tail_enabled(false);
    let registry = registry(&rt);
    let remembered = remember(&registry, CONTENT).await;
    let visibility_token = remembered["visibility_token"].clone();
    let seq = visibility_token["fences"][0]["ann_write_log_seq"]
        .as_u64()
        .expect("issued sequence");

    rt.sql()
        .writer()
        .await
        .expect("sql writer")
        .execute(SqlStatement {
            sql: "DELETE FROM ann_write_log WHERE seq = ?1".into(),
            params: vec![SqlValue::Integer(
                i64::try_from(seq).expect("SQLite sequence"),
            )],
            label: Some("session-visibility-remove-original-log-proof".into()),
        })
        .await
        .expect("remove original log proof");
    let remaining = rt
        .sql()
        .reader()
        .await
        .expect("sql reader")
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log WHERE seq = ?1".into(),
            params: vec![SqlValue::Integer(
                i64::try_from(seq).expect("SQLite sequence"),
            )],
            label: Some("session-visibility-check-original-log-proof".into()),
        })
        .await
        .expect("check original log proof");
    assert!(matches!(remaining, Some(SqlValue::Integer(0))));

    let eventual = registry
        .dispatch(
            "memory.recall",
            json!({"query": CONTENT, "limit": 50, "score_floor": 0.0}),
        )
        .await
        .expect("text recall still serves the matching memory");
    assert!(contains_id(&eventual, &remembered["id"]));

    let error = registry
        .dispatch("memory.recall", session_request(CONTENT, visibility_token))
        .await
        .expect_err("text or stale ANN hits cannot replace missing session proof");
    assert_freshness_unmet(error, MODEL);
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn failed_second_engine_cannot_turn_a_session_fence_into_healthy_engine_success() {
    let rt = vector_runtime();
    let registry = registry(&rt);
    let remembered = remember(&registry, CONTENT).await;
    rt.register_embedder(FailingEmbedProvider);

    let eventual = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": CONTENT,
                "fusion_strategy": "vector_only",
                "limit": 50,
                "score_floor": 0.0,
            }),
        )
        .await
        .expect("the healthy vector engine still serves eventual recall");
    assert!(contains_id(&eventual, &remembered["id"]));

    let error = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": CONTENT,
                "fusion_strategy": "vector_only",
                "limit": 50,
                "score_floor": 0.0,
                "consistency": "session",
                "visibility_token": {
                    "version": 1,
                    "namespace": "local",
                    "fences": [{"model": FAILING_MODEL, "ann_write_log_seq": 1}],
                },
                "timeout_ms": 0,
            }),
        )
        .await
        .expect_err("a failed fenced model must not inherit the healthy model's success");
    assert_freshness_unmet(error, FAILING_MODEL);
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn empty_fences_allow_text_only_session_recall() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let registry = registry(&rt);
    let remembered = remember(&registry, CONTENT).await;
    let visibility_token = remembered["visibility_token"].clone();
    assert_eq!(visibility_token["version"], 1);
    assert_eq!(visibility_token["namespace"], "local");
    assert_eq!(visibility_token["fences"], json!([]));

    let recalled = registry
        .dispatch("memory.recall", session_request(CONTENT, visibility_token))
        .await
        .expect("an empty fence has no vector visibility obligation");
    assert!(
        contains_id(&recalled, &remembered["id"]),
        "text-only session recall omitted the matching memory: {recalled:?}"
    );
}

#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn moved_keyed_replay_proves_destination_before_and_after_ann_publication() {
    use khive_db::namespace_move::{move_namespace, MoveRequest, MoveRoute, SubjectClass};

    let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
    backend.prepare_core_schema().expect("core schema");
    let rt = KhiveRuntime::from_backend(backend.clone(), RuntimeConfig::no_embeddings());
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 16,
    });
    let (registry, shared_ann) = registry_with_ann(&rt);
    let source = "visibility-move-source";
    let target = "visibility-move-target";
    let identity = |namespace: &str| RequestIdentity {
        namespace: namespace.to_owned(),
        ..Default::default()
    };
    let remember_args = |namespace: &str| {
        json!({
            "content": CONTENT,
            "memory_type": "semantic",
            "namespace": namespace,
            "key": "moved-visibility-key",
        })
    };
    let target_session_request = |visibility_token: Value| {
        let mut request = session_request(CONTENT, visibility_token);
        request["namespace"] = json!(target);
        request
    };
    let original = registry
        .dispatch_with_identity(
            "memory.remember",
            remember_args(source),
            Some(identity(source)),
        )
        .await
        .expect("keyed source remember");
    let old_seq = original["visibility_token"]["fences"][0]["ann_write_log_seq"]
        .as_u64()
        .expect("original write fence");
    let destination_seq: i64 = {
        let connection = backend.pool().writer().expect("move writer");
        let request = MoveRequest::new(
            source,
            vec![MoveRoute {
                class: SubjectClass::Note("memory".to_owned()),
                target: target.to_owned(),
            }],
        );
        connection.transaction(|conn| {
            move_namespace(conn, &request).expect("move keyed memory and vector");
            let seq = conn.query_row(
                "SELECT seq FROM ann_write_log WHERE namespace = ?1 AND subject_id = ?2 \
                 AND embedding_model = ?3 AND kind = 'note' AND field = 'note.content' AND op = 'upsert'",
                [target, original["id"].as_str().unwrap(), MODEL],
                |row| row.get::<_, i64>(0),
            )?;
            Ok(seq)
        }).expect("move transaction")
    };
    assert!(u64::try_from(destination_seq).unwrap() > old_seq);
    let log_rows_before_replay = {
        let connection = backend.pool().reader().expect("log reader");
        connection
            .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("log count")
    };
    let replay = registry
        .dispatch_with_identity(
            "memory.remember",
            remember_args(target),
            Some(identity(target)),
        )
        .await
        .expect("exact keyed replay in destination");
    assert_eq!(replay["id"], original["id"]);
    assert_eq!(replay["replayed"], true);
    let receipt = replay["visibility_token"].clone();
    assert_eq!(receipt["namespace"], target);
    assert_eq!(receipt["fences"][0]["ann_write_log_seq"], destination_seq);
    {
        let connection = backend.pool().reader().expect("replay log reader");
        let log_rows_after_replay: i64 = connection
            .query_row("SELECT COUNT(*) FROM ann_write_log", [], |row| row.get(0))
            .expect("post-replay log count");
        assert_eq!(
            log_rows_after_replay, log_rows_before_replay,
            "replay writes no replacement log row"
        );
    }
    let target_token = rt
        .authorize(Namespace::parse(target).unwrap())
        .expect("target token");
    assert_eq!(
        ann::bridge_applied_seq(&shared_ann, &AnnKey::from_token(MODEL)).await,
        None
    );
    let unscoped = registry
        .dispatch_with_identity(
            "memory.recall",
            session_request(CONTENT, receipt.clone()),
            Some(identity(target)),
        )
        .await
        .expect_err("an identity namespace alone does not widen default read visibility");
    assert!(matches!(
        unscoped,
        RuntimeError::InvalidInput(ref message)
            if message.contains("visibility_token namespace is not caller-visible")
    ));
    let recalled = registry
        .dispatch_with_identity(
            "memory.recall",
            target_session_request(receipt.clone()),
            Some(identity(target)),
        )
        .await
        .expect("unapplied destination row proves its exact live tail snapshot");
    assert!(contains_id(&recalled, &original["id"]));

    ann::ensure_ann_for_model(&rt, &target_token, &shared_ann, MODEL)
        .await
        .expect("publish moved vector");
    let applied = ann::bridge_applied_seq(&shared_ann, &AnnKey::from_token(MODEL))
        .await
        .expect("published watermark");
    assert!(applied >= u64::try_from(destination_seq).unwrap());
    ann::compact_log_for_test(&rt, MODEL)
        .await
        .expect("compact published move log");
    {
        let connection = backend.pool().reader().expect("compacted log reader");
        let remaining: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM ann_write_log WHERE seq = ?1",
                [destination_seq],
                |row| row.get(0),
            )
            .expect("compacted destination row count");
        assert_eq!(
            remaining, 0,
            "post-publication arm must use the watermark proof"
        );
    }
    let recalled = registry
        .dispatch_with_identity(
            "memory.recall",
            target_session_request(receipt),
            Some(identity(target)),
        )
        .await
        .expect("published destination fence remains provable after compaction");
    assert!(contains_id(&recalled, &original["id"]));
}
