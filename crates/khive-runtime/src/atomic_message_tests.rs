use super::*;
use async_trait::async_trait;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService, MAX_TEXT_BYTES};
use std::sync::{Arc, Mutex};

use khive_types::Namespace;

use crate::embedder_registry::EmbedderProvider;

const NAN_MODEL: &str = "atomic-message-nan-model";
const NAN_DIMS: usize = 4;

struct NanService;
#[async_trait]
impl EmbeddingService for NanService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![f32::NAN; NAN_DIMS]).collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        NAN_MODEL
    }
}
struct NanProvider;
#[async_trait]
impl EmbedderProvider for NanProvider {
    fn name(&self) -> &str {
        NAN_MODEL
    }
    fn dimensions(&self) -> usize {
        NAN_DIMS
    }
    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(NanService))
    }
}

struct TruncationService;
#[async_trait]
impl EmbeddingService for TruncationService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|_| vec![0.5; EmbeddingModel::MultilingualE5Base.dimensions()])
            .collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "multilingual-e5-base"
    }
}

struct TruncationProvider;
#[async_trait]
impl EmbedderProvider for TruncationProvider {
    fn name(&self) -> &str {
        "multilingual-e5-base"
    }
    fn dimensions(&self) -> usize {
        EmbeddingModel::MultilingualE5Base.dimensions()
    }
    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(TruncationService))
    }
}

const DEDUP_DIMS: usize = 4;

struct DedupService {
    name: &'static str,
}

#[async_trait]
impl EmbeddingService for DedupService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25; DEDUP_DIMS]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct DedupProvider {
    name: &'static str,
}

#[async_trait]
impl EmbedderProvider for DedupProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn dimensions(&self) -> usize {
        DEDUP_DIMS
    }

    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(DedupService { name: self.name }))
    }
}

struct BatchCountingService {
    name: &'static str,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl EmbeddingService for BatchCountingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.lock().unwrap().push(texts.to_vec());
        Ok(texts.iter().map(|_| vec![0.25; DEDUP_DIMS]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

struct BatchCountingProvider {
    name: &'static str,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
}

#[async_trait]
impl EmbedderProvider for BatchCountingProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn dimensions(&self) -> usize {
        DEDUP_DIMS
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(BatchCountingService {
            name: self.name,
            calls: Arc::clone(&self.calls),
        }))
    }
}

#[tokio::test]
async fn heterogeneous_notes_batch_models_with_one_schema_writer() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(Namespace::parse("batch-notes").unwrap())
        .unwrap();
    let names = ["batch-notes-a", "batch-notes-b"];
    let mut counters = Vec::new();
    for name in names {
        let calls = Arc::new(Mutex::new(Vec::new()));
        runtime.register_embedder(BatchCountingProvider {
            name,
            calls: Arc::clone(&calls),
        });
        runtime.embedder_with_token(&token, name).await.unwrap();
        counters.push(calls);
    }
    let inputs = [
        ("first", "shared text", true, 0.2),
        ("second", "shared text", true, 0.4),
        ("third", "different text", true, 0.6),
        ("disabled", "shared text", false, 0.8),
    ];
    let requests = inputs
        .iter()
        .map(|&(key, content, embed, salience)| AtomicNoteRequest {
            spec: AtomicNoteSpec {
                token: &token,
                id: None,
                kind: "observation",
                name: None,
                content,
                properties: Some(serde_json::json!({"key": key})),
            },
            options: AtomicNoteOptions {
                key: Some(key),
                salience: Some(salience),
                embed: Some(embed),
                ..Default::default()
            },
        })
        .collect();
    let before = runtime.backend().pool().writer_acquisition_snapshot();
    let prepared = prepare_atomic_note_requests(&runtime, requests)
        .await
        .unwrap();
    let after = runtime.backend().pool().writer_acquisition_snapshot();
    assert_eq!(after.acquisitions - before.acquisitions, 1);
    for calls in &counters {
        assert_eq!(
            *calls.lock().unwrap(),
            vec![vec!["shared text".to_owned(), "different text".to_owned()]],
        );
    }
    for (note, &(key, content, _, salience)) in prepared.notes.iter().zip(&inputs) {
        assert_eq!(note.key.as_deref(), Some(key));
        assert_eq!(note.content, content);
        assert_eq!(note.salience, Some(salience));
        assert_eq!(note.properties.as_ref().unwrap()["key"], key);
    }
    assert!(matches!(
        crate::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), prepared.plans)
            .await
            .unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    for name in names {
        assert_eq!(
            runtime
                .vectors_for_model(&token, name)
                .unwrap()
                .count()
                .await
                .unwrap(),
            3,
            "disabled notes must not inherit a sibling's shared vector",
        );
    }
}

#[tokio::test]
async fn atomic_note_batch_chunks_only_above_provider_limit() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    runtime.register_embedder(BatchCountingProvider {
        name: "batch-notes-large",
        calls: Arc::clone(&calls),
    });
    let contents: Vec<_> = (0..=lattice_embed::DEFAULT_MAX_BATCH_SIZE)
        .map(|index| format!("distinct document {index}"))
        .collect();
    let specs = contents
        .iter()
        .map(|content| AtomicNoteSpec {
            token: &token,
            id: None,
            kind: "observation",
            name: None,
            content,
            properties: None,
        })
        .collect();
    let prepared = prepare_atomic_notes(&runtime, specs, AtomicNoteOptions::default())
        .await
        .unwrap();
    assert_eq!(prepared.notes.len(), contents.len());
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].len(), lattice_embed::DEFAULT_MAX_BATCH_SIZE);
    assert_eq!(calls[1].len(), 1);
    assert_eq!(calls.concat(), contents);
}

/// A minimal note-write validator standing in for a pack's real one (e.g.
/// `khive-pack-comm`'s `derive_message_identity`): unconditionally stamps
/// `from_actor` with the caller's token-derived actor id, discarding
/// whatever the caller's own `properties` named for that key.
fn stamp_from_actor(
    _kind: &str,
    actor_id: &str,
    properties: Option<Value>,
) -> RuntimeResult<Option<Value>> {
    let mut props = match properties {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    props.insert(
        "from_actor".to_string(),
        Value::String(actor_id.to_string()),
    );
    Ok(Some(Value::Object(props)))
}

async fn fts_row_count(runtime: &KhiveRuntime, namespace: &str) -> i64 {
    let mut reader = runtime.sql().reader().await.expect("sql reader");
    match reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1".to_string(),
            params: vec![SqlValue::Text(namespace.to_string())],
            label: None,
        })
        .await
        .expect("fts count query")
    {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected fts count result: {other:?}"),
    }
}

async fn ann_write_log_count(runtime: &KhiveRuntime, namespace: &str, model: &str) -> i64 {
    let mut reader = runtime.sql().reader().await.expect("sql reader");
    match reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM ann_write_log \
                      WHERE namespace = ?1 AND embedding_model = ?2"
                .to_string(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(model.to_string()),
            ],
            label: None,
        })
        .await
        .expect("ann_write_log count query")
    {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected ann_write_log count result: {other:?}"),
    }
}

/// A custom embedding provider returning NaN must be rejected BEFORE any
/// plan is built — same validation `VectorStore::insert` performs on the
/// canonical `create_note_inner` path. No note row, FTS document, vector
/// row, or `ann_write_log` row may be committed.
#[tokio::test]
async fn create_notes_atomic_rejects_non_finite_embedding_before_any_write() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.register_embedder(NanProvider);
    let ns = "atomic-message-nan-test";
    let token = runtime
        .authorize(Namespace::parse(ns).unwrap())
        .expect("authorize");

    let result = create_notes_atomic(
        &runtime,
        vec![AtomicNoteSpec {
            token: &token,
            id: None,
            kind: "observation",
            name: None,
            content: "nan embedding content",
            properties: None,
        }],
    )
    .await;

    assert!(
        result.is_err(),
        "a non-finite embedding must be rejected; got {result:?}"
    );

    let alive = runtime
        .list_notes(&token, Some("observation"), 100, 0)
        .await
        .expect("list_notes")
        .into_iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        alive, 0,
        "no note row may be committed when an embedding is non-finite"
    );

    assert_eq!(
        fts_row_count(&runtime, ns).await,
        0,
        "no FTS document may be committed when an embedding is non-finite"
    );

    let vs = runtime
        .vectors_for_model(&token, NAN_MODEL)
        .expect("vec store");
    assert_eq!(
        vs.count().await.expect("count"),
        0,
        "no vector row may be committed when an embedding is non-finite"
    );

    assert_eq!(
        ann_write_log_count(&runtime, ns, NAN_MODEL).await,
        0,
        "no ann_write_log row may be committed when an embedding is non-finite"
    );
}

#[tokio::test]
async fn create_notes_atomic_rejects_reserved_secret_gate_key() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = "atomic-message-reserved-key-test";
    let token = runtime
        .authorize(Namespace::parse(ns).unwrap())
        .expect("authorize");

    let result = create_notes_atomic(
        &runtime,
        vec![AtomicNoteSpec {
            token: &token,
            id: None,
            kind: "observation",
            name: None,
            content: "atomic message reservation target",
            properties: Some(
                serde_json::json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"}),
            ),
        }],
    )
    .await;

    let err = result.expect_err("caller-supplied reserved key must be rejected");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
        "unexpected error: {err:?}"
    );

    let alive = runtime
        .list_notes(&token, Some("observation"), 100, 0)
        .await
        .expect("list_notes")
        .into_iter()
        .filter(|n| n.deleted_at.is_none())
        .count();
    assert_eq!(
        alive, 0,
        "no note row may be committed on reservation rejection"
    );
}

#[tokio::test]
async fn create_notes_atomic_with_report_preserves_truncation_outcome() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.register_embedder(TruncationProvider);
    let token = runtime
        .authorize(Namespace::parse("atomic-message-truncation-test").unwrap())
        .expect("authorize");
    let content = "x".repeat(MAX_TEXT_BYTES);

    let (notes, report) = create_notes_atomic_with_report(
        &runtime,
        vec![AtomicNoteSpec {
            token: &token,
            id: None,
            kind: "observation",
            name: None,
            content: &content,
            properties: None,
        }],
    )
    .await
    .expect("atomic note write");

    assert_eq!(notes.len(), 1);
    assert_eq!(report.truncated, 1);
    assert_eq!(
        report.discarded_bytes,
        "passage: ".len() as u64,
        "E5 document-prefix reservation must be reflected in the returned report"
    );
}

#[tokio::test]
async fn create_notes_atomic_reuses_identical_content_once_per_model() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    for name in ["atomic-dedup-model-a", "atomic-dedup-model-b"] {
        runtime.register_embedder(DedupProvider { name });
    }
    let outbound_token = runtime
        .authorize(Namespace::parse("atomic-dedup-outbound").unwrap())
        .expect("authorize outbound");
    let inbound_token = runtime
        .authorize(Namespace::parse("atomic-dedup-inbound").unwrap())
        .expect("authorize inbound");
    let content = "byte-identical outbound and inbound message content";

    let usage = crate::usage::UsageContext::new();
    let notes = crate::usage::scope(usage.clone(), async {
        create_notes_atomic(
            &runtime,
            vec![
                AtomicNoteSpec {
                    token: &outbound_token,
                    id: None,
                    kind: "observation",
                    name: Some("shared subject"),
                    content,
                    properties: Some(serde_json::json!({"direction": "outbound"})),
                },
                AtomicNoteSpec {
                    token: &inbound_token,
                    id: None,
                    kind: "observation",
                    name: Some("shared subject"),
                    content,
                    properties: Some(serde_json::json!({"direction": "inbound"})),
                },
            ],
        )
        .await
    })
    .await
    .expect("atomic note pair");

    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].content, content);
    assert_eq!(notes[1].content, content);
    assert_eq!(
        usage.snapshot()["embed_calls"],
        2,
        "two identical notes across two models must issue one embed per model"
    );
    for model in ["atomic-dedup-model-a", "atomic-dedup-model-b"] {
        for (direction, token) in [("outbound", &outbound_token), ("inbound", &inbound_token)] {
            assert_eq!(
                runtime
                    .vectors_for_model(token, model)
                    .expect("vector store")
                    .count()
                    .await
                    .expect("vector count"),
                1,
                "the {direction} note must retain its vector row for {model}"
            );
        }
    }
}

/// Calling `create_notes_atomic` twice with the SAME caller-supplied id
/// (different content each time) must leave exactly one FTS document for
/// that id, reflecting the latest content — not an append of two
/// documents (the note row is already an upsert; the FTS half must match).
#[tokio::test]
async fn version_create_notes_atomic_upserts_fts_document_for_reused_note_id() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let ns = "atomic-message-fts-upsert-test";
    let token = runtime
        .authorize(Namespace::parse(ns).unwrap())
        .expect("authorize");
    let id = Uuid::new_v4();

    let first = create_notes_atomic(
        &runtime,
        vec![AtomicNoteSpec {
            token: &token,
            id: Some(id),
            kind: "observation",
            name: None,
            content: "first content",
            properties: None,
        }],
    )
    .await
    .expect("first write with supplied id");
    assert_eq!(first[0].version, 1);

    let second = create_notes_atomic(
        &runtime,
        vec![AtomicNoteSpec {
            token: &token,
            id: Some(id),
            kind: "observation",
            name: None,
            content: "second content replacing the first",
            properties: None,
        }],
    )
    .await
    .expect("second write reusing the same id");
    assert_eq!(second[0].version, 2);
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
            .version,
        2
    );
    let repeated = create_notes_atomic(
        &runtime,
        (0..2)
            .map(|_| AtomicNoteSpec {
                token: &token,
                id: Some(id),
                kind: "observation",
                name: None,
                content: "second content replacing the first",
                properties: None,
            })
            .collect(),
    )
    .await
    .expect("equal-value upserts in the same batch");
    assert_eq!(
        repeated.iter().map(|note| note.version).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
            .version,
        4
    );

    assert_eq!(
        fts_row_count(&runtime, ns).await,
        1,
        "exactly one FTS document may exist for the reused id"
    );

    let text = runtime.text_for_notes(&token).expect("text store");
    let doc = text
        .get_document(ns, id)
        .await
        .expect("get_document")
        .expect("document exists for the reused id");
    assert!(
        doc.body.contains("second content"),
        "the surviving FTS document must reflect the latest content; got {:?}",
        doc.body
    );
}

/// ADR-124 regression: `create_notes_atomic_with_report` must run
/// `derive_note_write_properties` per spec, the same as every other
/// note-write site (`operations.rs`'s create funnel,
/// `atomic_prepare::prepare_add_note`). A caller holding a valid token
/// for one actor must not be able to plant an arbitrary `from_actor` (or
/// any other owner-established property) via `AtomicNoteSpec::properties`
/// — the installed validator's derived value must win.
///
/// This test fails if the `derive_note_write_properties` call in
/// `create_notes_atomic_with_report`'s spec loop is removed: verified by
/// deleting that call in a scratch copy of this file and re-running —
/// the assertion below turns red because the stored `from_actor` reverts
/// to the forged `"forged-actor"` value instead of the token's actor id.
#[tokio::test]
async fn create_notes_atomic_derives_from_actor_overwriting_a_forged_value() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.install_note_write_validator(std::sync::Arc::new(stamp_from_actor));
    let ns = "atomic-message-identity-guard-test";
    let token = runtime
        .authorize(Namespace::parse(ns).unwrap())
        .expect("authorize");
    let true_actor = token.actor().id.clone();

    let notes = create_notes_atomic(
        &runtime,
        vec![AtomicNoteSpec {
            token: &token,
            id: None,
            kind: "observation",
            name: None,
            content: "atomic writer identity guard probe",
            properties: Some(serde_json::json!({"from_actor": "forged-actor"})),
        }],
    )
    .await
    .expect("atomic note write must succeed — the guard derives, it does not refuse");

    assert_eq!(notes.len(), 1);
    assert_eq!(
        notes[0]
            .properties
            .as_ref()
            .and_then(|p| p.get("from_actor"))
            .and_then(|v| v.as_str()),
        Some(true_actor.as_str()),
        "the atomic writer must store the token-derived from_actor, not the \
             caller-supplied forged value"
    );
}
