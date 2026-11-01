use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use lattice_embed::EmbeddingService;

use crate::{
    test_support::{with_receipt_credentials, HashVecProvider},
    MemoryPack,
};

/// `memory.remember` must persist exactly ONE `NoteCreated` event carrying
/// the calling actor, the new note's id as `target_id`, and
/// `substrate = Note` so `decode_target_observation` resolves it as a `Note`
/// referent. That event is now the runtime's, emitted once on the single
/// note-create funnel. This pack used to emit a second one of its own beside
/// it; the count assertion below is what makes that duplicate fail, so it is
/// load-bearing rather than incidental.
#[tokio::test]
async fn remember_persists_note_created_event_with_target() {
    let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    let result = registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "memory_remembered event payload coverage",
                "memory_type": "semantic",
            }),
        )
        .await
        .expect("memory.remember must succeed");
    let note_id: uuid::Uuid = result["id"]
        .as_str()
        .expect("response must carry id")
        .parse()
        .expect("id must be a uuid");

    let store = rt.events(&token).expect("event store");
    let page = store
        .query_events(
            khive_storage::event::EventFilter {
                kinds: vec![khive_types::EventKind::NoteCreated],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query_events");

    assert_eq!(
        page.items.len(),
        1,
        "exactly one NoteCreated event must be persisted: {page:?}"
    );
    let event = &page.items[0];
    assert_eq!(event.verb, "create");
    assert_eq!(
        event.actor,
        format!("{}:{}", token.actor().kind, token.actor().id)
    );
    assert_eq!(event.target_id, Some(note_id));
    assert_eq!(event.substrate, khive_types::SubstrateKind::Note);
    assert_eq!(event.payload["kind"], serde_json::json!("memory"));
    // `memory_type` is a property of the note the event targets, not a copy
    // in the event payload: the runtime emitter knows the note, not the verb
    // that asked for it. The response is the caller-facing surface for it.
    assert_eq!(result["memory_type"], serde_json::json!("semantic"));
    let receipt = rt
        .open_visibility_receipt(
            result["visibility_token"].as_str().expect("opaque receipt"),
            &["local"],
            &[],
        )
        .expect("a text-only memory has an authenticated empty vector fence set");
    assert_eq!(receipt.namespace(), "local");
    assert_eq!(receipt.sequence_for_model("unwritten-model"), None);
}

#[tokio::test]
async fn remember_returns_the_vector_writes_transactional_ann_sequence() {
    const MODEL: &str = "remember-visibility-test-model";
    let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 8,
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    let result = registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "transactional visibility receipt for one memory vector",
                "memory_type": "semantic",
            }),
        )
        .await
        .expect("remember vector");
    let token = result["visibility_token"].as_str().expect("opaque receipt");
    let receipt = rt
        .open_visibility_receipt(token, &["local"], &[MODEL.to_owned()])
        .expect("authenticated one-model receipt");
    assert_eq!(receipt.namespace(), "local");
    let seq = receipt
        .sequence_for_model(MODEL)
        .expect("positive log sequence");
    assert!(seq > 0);
    for hidden_field in [
        "ann_write_log_seq",
        "issued_at",
        "fences",
        "namespace",
        "model",
    ] {
        assert!(
            result.get(hidden_field).is_none(),
            "receipt surface exposed {hidden_field}"
        );
    }
    assert!(
        !token.contains(MODEL),
        "opaque receipt exposed the model sentinel"
    );

    // AUTOINCREMENT retains the committed high sequence even if an ANN
    // checkpoint compacts the individual log row before this assertion.
    let mut reader = rt.sql().reader().await.expect("sql reader");
    let observed = reader
        .query_scalar(SqlStatement {
            sql: "SELECT seq FROM sqlite_sequence WHERE name = 'ann_write_log'".into(),
            params: vec![],
            label: Some("remember-visibility-sequence-test".into()),
        })
        .await
        .expect("read sequence");
    assert!(matches!(observed, Some(SqlValue::Integer(value)) if value == seq as i64));
}

#[tokio::test]
async fn remember_replay_returns_original_without_creating_a_duplicate() {
    let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    let args = serde_json::json!({
        "content": "idempotent memory content",
        "memory_type": "semantic",
        "idempotency_key": "remember-replay",
    });
    let first = registry
        .dispatch("memory.remember", args.clone())
        .await
        .expect("first remember");
    let second = registry
        .dispatch("memory.remember", args)
        .await
        .expect("identical replay");

    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["replayed"], serde_json::json!(true));
    assert_ne!(
        second["visibility_token"], first["visibility_token"],
        "replay must reseal with a fresh nonce"
    );
    for response in [&first, &second] {
        let receipt = rt
            .open_visibility_receipt(
                response["visibility_token"]
                    .as_str()
                    .expect("opaque receipt"),
                &["local"],
                &[],
            )
            .expect("authenticated zero-model replay receipt");
        assert_eq!(receipt.namespace(), "local");
    }
    let notes = rt
        .notes(&token)
        .expect("note store")
        .get_live_notes_by_key("local", "remember-replay", Some("memory"))
        .await
        .expect("key lookup");
    assert_eq!(notes.len(), 1);

    let conflict = registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "different content",
                "memory_type": "semantic",
                "idempotency_key": "remember-replay",
            }),
        )
        .await
        .expect_err("different content under one key must refuse");
    assert!(conflict.to_string().contains("idempotency_key_conflict"));
    assert!(conflict.to_string().contains("remember-replay"));
    let notes_after = rt
        .notes(&token)
        .expect("note store")
        .get_live_notes_by_key("local", "remember-replay", Some("memory"))
        .await
        .expect("key lookup after refused replay");
    assert_eq!(notes_after.len(), 1);
    assert_eq!(notes_after[0].content, "idempotent memory content");
}

#[tokio::test]
async fn remember_replay_answers_from_the_stored_memory() {
    // #2700: a replay must carry the stored note's annotation edge and the
    // memory_type it was written with, never the replay request's values.
    let rt = with_receipt_credentials(KhiveRuntime::memory().expect("in-memory runtime"));
    let ns = Namespace::parse("local").expect("local namespace");
    let token = rt.authorize(ns).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("registry");

    let source = rt
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "replay source",
            None,
            None,
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .expect("source entity");
    let args = serde_json::json!({
        "content": "replayed memory answers from the store",
        "memory_type": "semantic",
        "namespace": "local",
        "source_id": source.id.to_string(),
        "idempotency_key": "remember-replay-stored",
    });
    let first = registry
        .dispatch("memory.remember", args.clone())
        .await
        .expect("first remember");
    let first_edge = first["edge_id"]
        .as_str()
        .expect("original keyed create reports its annotation edge")
        .to_owned();
    assert_eq!(first["memory_type"], serde_json::json!("semantic"));

    // Exact replay: same edge, same type, flagged as a replay.
    let exact = registry
        .dispatch("memory.remember", args)
        .await
        .expect("exact replay");
    assert_eq!(exact["id"], first["id"]);
    assert_eq!(exact["replayed"], serde_json::json!(true));
    assert_eq!(exact["edge_id"], serde_json::json!(first_edge));
    assert_eq!(exact["memory_type"], serde_json::json!("semantic"));

    // Same key and content, different type and no source_id: the response
    // still describes the stored memory.
    let drifted = registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "replayed memory answers from the store",
                "memory_type": "episodic",
                "namespace": "local",
                "idempotency_key": "remember-replay-stored",
            }),
        )
        .await
        .expect("replay with a drifted type");
    assert_eq!(drifted["id"], first["id"]);
    assert_eq!(drifted["replayed"], serde_json::json!(true));
    assert_eq!(drifted["memory_type"], serde_json::json!("semantic"));
    assert_eq!(drifted["edge_id"], serde_json::json!(first_edge));
    let notes = rt
        .notes(&token)
        .expect("note store")
        .get_live_notes_by_key("local", "remember-replay-stored", Some("memory"))
        .await
        .expect("key lookup");
    assert_eq!(notes.len(), 1);
    let stored_type = notes[0]
        .properties
        .as_ref()
        .and_then(|pr| pr.get("memory_type"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    assert_eq!(stored_type.as_deref(), Some("semantic"));

    let alternate = rt
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "alternate source",
            None,
            None,
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .expect("alternate entity");
    let missing = uuid::Uuid::new_v4();
    for replay_source in [alternate.id, missing] {
        let replay = registry
            .dispatch(
                "memory.remember",
                serde_json::json!({
                    "content": "replayed memory answers from the store",
                    "memory_type": "semantic",
                    "namespace": "local",
                    "source_id": replay_source.to_string(),
                    "idempotency_key": "remember-replay-stored",
                }),
            )
            .await
            .expect("replay ignores a changed or missing full UUID source");
        assert_eq!(replay["id"], first["id"]);
        assert_eq!(replay["replayed"], serde_json::json!(true));
        assert_eq!(replay["edge_id"], serde_json::json!(first_edge));
    }
    let mut reader = rt.sql().reader().await.expect("sql reader");
    let edge_count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM graph_edges WHERE source_id = ?1".into(),
            params: vec![SqlValue::Text(first["id"].as_str().unwrap().into())],
            label: Some("remember-replay-edge-count".into()),
        })
        .await
        .expect("count original memory edges");
    assert!(matches!(edge_count, Some(SqlValue::Integer(1))));
    drop(reader);

    assert!(rt
        .delete_entity(&token, source.id, true)
        .await
        .expect("delete source"));
    let mut reader = rt.sql().reader().await.expect("sql reader");
    let before = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM graph_edges WHERE source_id = ?1".into(),
            params: vec![SqlValue::Text(first["id"].as_str().unwrap().into())],
            label: Some("remember-replay-edges-before".into()),
        })
        .await
        .expect("count edges after source deletion");
    drop(reader);
    let replay = registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "replayed memory answers from the store",
                "memory_type": "semantic",
                "namespace": "local",
                "source_id": source.id.to_string(),
                "idempotency_key": "remember-replay-stored",
            }),
        )
        .await
        .expect("replay after deletion of original source");
    assert_eq!(replay["id"], first["id"]);
    assert_eq!(replay["replayed"], serde_json::json!(true));
    let surviving_edges = rt
        .neighbors_with_query(
            &token,
            first["id"].as_str().unwrap().parse().unwrap(),
            khive_storage::types::NeighborQuery {
                direction: khive_storage::types::Direction::Out,
                relations: Some(vec![khive_storage::EdgeRelation::Annotates]),
                limit: None,
                min_weight: None,
            },
        )
        .await
        .expect("read surviving original annotation");
    let surviving_edge = surviving_edges.first().map(|hit| hit.edge_id.to_string());
    if let Some(edge) = &surviving_edge {
        assert_eq!(edge, &first_edge);
    }
    assert_eq!(
        replay.get("edge_id").and_then(|edge| edge.as_str()),
        surviving_edge.as_deref()
    );
    let mut reader = rt.sql().reader().await.expect("sql reader");
    let after = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM graph_edges WHERE source_id = ?1".into(),
            params: vec![SqlValue::Text(first["id"].as_str().unwrap().into())],
            label: Some("remember-replay-edges-after".into()),
        })
        .await
        .expect("count edges after replay");
    assert!(matches!((before, after),
        (Some(SqlValue::Integer(a)), Some(SqlValue::Integer(b))) if a == b));
    drop(reader);
    assert_eq!(
        rt.notes(&token)
            .unwrap()
            .get_live_notes_by_key("local", "remember-replay-stored", Some("memory"))
            .await
            .unwrap()
            .len(),
        1
    );
}
struct CountingService(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for CountingService {
    async fn embed(
        &self,
        texts: &[String],
        _: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
    }
    fn supports_model(&self, _: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "receipt-preflight-counter"
    }
}

struct CountingProvider {
    builds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    embeds: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for CountingProvider {
    fn name(&self) -> &str {
        "receipt-preflight-counter"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(std::sync::Arc::new(CountingService(self.embeds.clone())))
    }
}

async fn domain_counts(runtime: &KhiveRuntime) -> Vec<i64> {
    let mut reader = runtime.sql().reader().await.expect("domain count reader");
    let mut counts = Vec::new();
    for table in [
        "notes",
        "graph_edges",
        "vector_provenance",
        "ann_write_log",
        "memory_visibility_receipts",
        "memory_visibility_fences",
        "memory_visibility_epochs",
    ] {
        let count = reader
            .query_scalar(SqlStatement {
                sql: format!("SELECT COUNT(*) FROM {table}"),
                params: vec![],
                label: Some("receipt-preflight-domain-count".into()),
            })
            .await
            .expect("domain count");
        let Some(SqlValue::Integer(count)) = count else {
            panic!("integer domain count");
        };
        counts.push(count);
    }
    counts
}

async fn epoch_of(runtime: &KhiveRuntime, note_id: &str) -> Option<String> {
    let mut reader = runtime.sql().reader().await.expect("epoch reader");
    match reader
        .query_scalar(SqlStatement {
            sql: "SELECT epoch FROM memory_visibility_epochs WHERE note_id = ?1".into(),
            params: vec![SqlValue::Text(note_id.to_owned())],
            label: Some("receipt-custody-absent-epoch".into()),
        })
        .await
        .expect("epoch read")
    {
        Some(SqlValue::Text(epoch)) => Some(epoch),
        _ => None,
    }
}

#[tokio::test]
async fn missing_receipt_key_writes_receipt_rows_without_a_token_for_cold_and_cached_embedders() {
    use khive_runtime::RuntimeError;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    // One write per runtime pair keeps a background ANN warm from an earlier
    // write out of the way of the next write's custody preflight.
    for (cached, keyed) in [(false, true), (true, false), (true, true), (false, false)] {
        let build_registry = |rt: &KhiveRuntime| {
            let mut builder = VerbRegistryBuilder::new();
            builder.register(KgPack::new(rt.clone()));
            builder.register(MemoryPack::new(rt.clone()));
            builder.build().expect("registry")
        };
        let builds = Arc::new(AtomicUsize::new(0));
        let embeds = Arc::new(AtomicUsize::new(0));
        let rt = KhiveRuntime::memory().expect("runtime without receipt custody");
        rt.register_embedder(CountingProvider {
            builds: builds.clone(),
            embeds: embeds.clone(),
        });
        let registry = build_registry(&rt);
        // The same write against a runtime that holds custody defines what a
        // sealed write persists; only the sealing differs.
        let sealed = with_receipt_credentials(KhiveRuntime::memory().expect("sealed runtime"));
        sealed.register_embedder(CountingProvider {
            builds: Arc::new(AtomicUsize::new(0)),
            embeds: Arc::new(AtomicUsize::new(0)),
        });
        let sealed_registry = build_registry(&sealed);
        if cached {
            rt.embedder("receipt-preflight-counter")
                .await
                .expect("prime the real cached service");
        }
        assert_eq!(builds.load(Ordering::SeqCst), usize::from(cached));
        let before = domain_counts(&rt).await;
        let sealed_before = domain_counts(&sealed).await;
        let mut args = serde_json::json!({
            "content": format!("custody-absent write, cached embedder: {cached}"),
            "memory_type": "semantic",
        });
        if keyed {
            args["key"] = serde_json::json!("receipt-preflight");
        }
        let response = registry
            .dispatch("memory.remember", args.clone())
            .await
            .expect("absent custody must not refuse the write");
        assert_eq!(response["visibility_token"], serde_json::Value::Null);
        assert_eq!(
            response["visibility_token_reason"],
            "visibility_key_unavailable"
        );
        assert!(embeds.load(Ordering::SeqCst) > 0);
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        let sealed_response = sealed_registry
            .dispatch("memory.remember", args)
            .await
            .expect("sealed control write");
        assert!(sealed_response["visibility_token"].is_string());
        assert!(sealed_response.get("visibility_token_reason").is_none());

        // The ANN log is compacted in the background, so its row count is
        // not a stable measure; the rest are.
        let added = |rt_counts: Vec<i64>, before: &[i64]| -> Vec<i64> {
            let mut added: Vec<i64> = rt_counts.iter().zip(before).map(|(a, b)| a - b).collect();
            added.remove(3);
            added
        };
        let added_absent = added(domain_counts(&rt).await, &before);
        assert_eq!(
            added_absent,
            added(domain_counts(&sealed).await, &sealed_before),
            "keyed: {keyed}: only sealing differs from a custody-holding write"
        );
        if keyed {
            // The note, receipt header, model fence and `modern` marker each
            // gain exactly one row.
            assert_eq!(added_absent, [1, 0, 0, 1, 1, 1]);
            let id = response["id"].as_str().expect("memory id");
            assert_eq!(epoch_of(&rt, id).await.as_deref(), Some("modern"));
        } else {
            assert_eq!(added_absent, [1, 0, 0, 0, 0, 0]);
        }
    }
    let rt = KhiveRuntime::memory().expect("runtime without receipt custody");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    let before = domain_counts(&rt).await;
    for invalid in [
        serde_json::json!({"content": "valid", "key": "invalid\0key"}),
        serde_json::json!({"content": " ", "memory_type": "semantic"}),
        serde_json::json!({"content": "valid", "namespace": "bad\0namespace"}),
    ] {
        let error = registry
            .dispatch("memory.remember", invalid)
            .await
            .expect_err("invalid arguments still refuse without custody");
        assert!(matches!(
            error.refusal_source(),
            RuntimeError::InvalidInput(_)
        ));
    }
    let error = registry
        .dispatch(
            "memory.remember",
            serde_json::json!({
                "content": "valid",
                "memory_type": "semantic",
                "embedding_model": "unregistered-receipt-model",
            }),
        )
        .await
        .expect_err("model validation still refuses without custody");
    assert!(matches!(
        error.refusal_source(),
        RuntimeError::UnknownModel(_)
    ));
    assert_eq!(domain_counts(&rt).await, before);
}

struct FailingReceiptProvider {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    fail_on_call: usize,
}

impl khive_runtime::credentials::CredentialProvider for FailingReceiptProvider {
    fn resolve(
        &self,
        _: &str,
    ) -> Result<
        khive_runtime::credentials::CredentialMaterial,
        khive_runtime::credentials::CredentialError,
    > {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if call >= self.fail_on_call {
            return Err(khive_runtime::credentials::CredentialError::Unavailable {
                name: "provider-secret-sentinel".into(),
            });
        }
        Ok(khive_runtime::credentials::CredentialMaterial::new(
            vec![b'A'; 43],
        ))
    }
    fn cache_lifetime(&self) -> khive_runtime::credentials::CredentialCacheLifetime {
        khive_runtime::credentials::CredentialCacheLifetime::NoCache
    }
}

#[tokio::test]
async fn configured_key_that_cannot_be_resolved_still_refuses_before_any_write() {
    use khive_runtime::credentials::{
        CredentialConfig, CredentialKind, CredentialRegistry, VisibilityReceiptConfig,
        VisibilityReceiptKeyConfig,
    };
    use khive_runtime::RuntimeError;
    use khive_types::ErrorKind;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let calls = Arc::new(AtomicUsize::new(0));
    let mut credentials = CredentialRegistry::new(vec![CredentialConfig {
        name: "receipt-down".into(),
        kind: CredentialKind::SigningKey,
        provider: "receipt-down-provider".into(),
        env_var: None,
        header: None,
    }])
    .expect("credential declaration");
    credentials
        .register_provider(
            "receipt-down-provider".into(),
            Arc::new(FailingReceiptProvider {
                calls: calls.clone(),
                fail_on_call: 1,
            }),
        )
        .expect("outage provider");
    let rt = KhiveRuntime::memory()
        .expect("in-memory runtime")
        .with_visibility_receipt_credentials(
            VisibilityReceiptConfig {
                keys: vec![VisibilityReceiptKeyConfig {
                    id: "receipt-down-key".into(),
                    credential: "receipt-down".into(),
                    encrypt: true,
                }],
            },
            Arc::new(credentials),
        )
        .expect("receipt custody");
    let builds = Arc::new(AtomicUsize::new(0));
    let embeds = Arc::new(AtomicUsize::new(0));
    rt.register_embedder(CountingProvider {
        builds: builds.clone(),
        embeds: embeds.clone(),
    });
    assert_eq!(
        rt.registered_embedding_model_names(),
        vec!["receipt-preflight-counter".to_owned()]
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(rt.clone()));
    let registry = builder.build().expect("registry");
    let before = domain_counts(&rt).await;
    assert_eq!(before, [0; 7]);
    for key in [Some("receipt-down-key"), None] {
        let mut args = serde_json::json!({
            "content": "a configured key that cannot be resolved must not write",
            "memory_type": "semantic",
        });
        if let Some(key) = key {
            args["key"] = serde_json::json!(key);
        }
        let error = registry
            .dispatch("memory.remember", args)
            .await
            .expect_err("configured but unresolvable key must refuse");
        assert!(matches!(error.refusal_source(), RuntimeError::Khive(error)
                if error.kind() == ErrorKind::Unavailable
                    && error.details().and_then(|details| details.get("reason"))
                        == Some("visibility_key_unavailable")));
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        assert_eq!(embeds.load(Ordering::SeqCst), 0);
        assert_eq!(domain_counts(&rt).await, before);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn actual_seal_key_failure_retains_holder_and_unknown_disposition_on_create_and_replay() {
    use khive_runtime::credentials::{
        CredentialConfig, CredentialKind, CredentialRegistry, VisibilityReceiptConfig,
        VisibilityReceiptKeyConfig,
    };
    use khive_runtime::{DomainDisposition, RuntimeError};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    for replayed in [false, true] {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut credentials = CredentialRegistry::new(vec![CredentialConfig {
            name: "receipt-outage".into(),
            kind: CredentialKind::SigningKey,
            provider: "receipt-outage-provider".into(),
            env_var: None,
            header: None,
        }])
        .expect("credential declaration");
        credentials
            .register_provider(
                "receipt-outage-provider".into(),
                Arc::new(FailingReceiptProvider {
                    calls: calls.clone(),
                    fail_on_call: if replayed { 4 } else { 2 },
                }),
            )
            .expect("outage provider");
        let rt = rt
            .with_visibility_receipt_credentials(
                VisibilityReceiptConfig {
                    keys: vec![VisibilityReceiptKeyConfig {
                        id: "receipt-outage-key".into(),
                        credential: "receipt-outage".into(),
                        encrypt: true,
                    }],
                },
                Arc::new(credentials),
            )
            .expect("receipt custody");
        let token = rt.authorize(Namespace::local()).expect("local token");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(MemoryPack::new(rt.clone()));
        let registry = builder.build().expect("registry");
        let args = serde_json::json!({
            "content": "committed receipt outage memory",
            "memory_type": "semantic",
            "key": "receipt-outage"
        });
        let first = if replayed {
            Some(
                registry
                    .dispatch("memory.remember", args.clone())
                    .await
                    .expect("initial issuance"),
            )
        } else {
            None
        };
        let error = registry
            .dispatch("memory.remember", args)
            .await
            .expect_err("key becomes unavailable during actual issuance");
        assert_eq!(calls.load(Ordering::SeqCst), if replayed { 4 } else { 2 });
        let notes = rt
            .notes(&token)
            .expect("note store")
            .get_live_notes_by_key("local", "receipt-outage", Some("memory"))
            .await
            .expect("reconcile holder");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "committed receipt outage memory");
        if let Some(first) = first {
            assert_eq!(first["id"], notes[0].id.to_string());
        }
        let RuntimeError::Khive(domain) = error.refusal_source() else {
            panic!("typed issuance failure: {error:?}");
        };
        assert_eq!(
            domain.details().and_then(|details| details.get("reason")),
            Some("visibility_key_unavailable")
        );
        assert_eq!(
            domain
                .details()
                .and_then(|details| details.get("memory_id")),
            Some(notes[0].id.to_string().as_str())
        );
        assert_eq!(
            domain
                .details()
                .and_then(|details| details.get("receipt_phase")),
            Some(if replayed {
                "exact_replay"
            } else {
                "post_commit"
            })
        );
        let projected = khive_runtime::runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(projected["domain_disposition"], "unknown");
        assert_eq!(projected["retryable"], true);
        assert!(!projected.to_string().contains("provider-secret-sentinel"));
        assert!(!projected.to_string().contains("ann_write_log_seq"));
    }
}
