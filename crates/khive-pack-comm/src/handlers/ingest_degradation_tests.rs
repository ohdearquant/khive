use super::*;

#[tokio::test]
async fn ingest_dedup_hit_does_not_publish() {
    use khive_runtime::{AllowAllGate, BackendId, Namespace, RuntimeConfig};
    use uuid::Uuid;

    let ns = format!("ingest-dedup-{}", Uuid::new_v4().simple());
    let runtime = super::KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::parse(&ns).unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let token = runtime
        .authorize(Namespace::parse(&ns).unwrap())
        .expect("authorize");
    let signal = InboxSignal::new();

    // handle_ingest fails closed without a channel-ingest grant; mint one
    // directly rather than routing through a full pack registration.
    let capability = khive_runtime::ChannelIngestCapability::grant_for_direct_composition();

    let body = json!({
        "from": "email:sender@example.com",
        "to": "local",
        "content": "dedup probe",
        "external_id": "imap:long-poll:dedup:1",
    });

    let first = super::handle_ingest(
        &runtime,
        &signal,
        Some(&capability),
        &Ok(None),
        &token,
        body.clone(),
        std::time::Duration::from_secs(14 * 24 * 60 * 60),
    )
    .await
    .expect("first ingest succeeds");
    assert_eq!(first["deduplicated"].as_bool(), Some(false));
    let generation_after_commit = signal.snapshot();
    assert_ne!(
        generation_after_commit, 0,
        "a newly committed ingest must publish a wake"
    );

    let second = super::handle_ingest(
        &runtime,
        &signal,
        Some(&capability),
        &Ok(None),
        &token,
        body,
        std::time::Duration::from_secs(14 * 24 * 60 * 60),
    )
    .await
    .expect("deduplicated ingest succeeds");
    assert_eq!(second["deduplicated"].as_bool(), Some(true));
    assert_eq!(
        signal.snapshot(),
        generation_after_commit,
        "a deduplicated ingest must not publish a wake"
    );
}

fn fixture() -> (
    KhiveRuntime,
    NamespaceToken,
    InboxSignal,
    khive_runtime::ChannelIngestCapability,
) {
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "comm".into()],
        brain_profile: None,
        actor_id: None,
        default_namespace: khive_runtime::Namespace::local(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let capability = khive_runtime::ChannelIngestCapability::grant_for_direct_composition();
    (runtime, token, InboxSignal::new(), capability)
}

fn message() -> Value {
    json!({
        "from": "email:sender@example.com", "to": "local",
        "content": "committed ingest index diagnostic",
        "channel_kind": "email", "channel_slug": "mailbox@example.com",
        "external_id": "imap:partial:1:7",
    })
}

fn install_fts_refusal(runtime: &KhiveRuntime, token: &NamespaceToken) {
    runtime.text_for_notes(token).unwrap();
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_ingest_fts BEFORE INSERT ON fts_notes_rowids \
         BEGIN SELECT RAISE(ABORT, 'comm ingest FTS refusal'); END;",
        )
        .unwrap();
}

async fn ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    signal: &InboxSignal,
    capability: &khive_runtime::ChannelIngestCapability,
    body: Value,
) -> Result<Value, RuntimeError> {
    handle_ingest(
        runtime,
        signal,
        Some(capability),
        &Ok(None),
        token,
        body,
        std::time::Duration::from_secs(60),
    )
    .await
}

struct CountingProvider(std::sync::Arc<std::sync::atomic::AtomicUsize>);
struct CountingService(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl khive_runtime::embedder_registry::EmbedderProvider for CountingProvider {
    fn name(&self) -> &str {
        "ingest-counting"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(
        &self,
    ) -> khive_runtime::RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(std::sync::Arc::new(CountingService(self.0.clone())))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for CountingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.0
            .fetch_add(texts.len(), std::sync::atomic::Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0; 4]).collect())
    }
    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "ingest-counting"
    }
}

async fn assert_degraded_ingest(with_attachment: bool) {
    use khive_storage::BlobStore as _;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let (runtime, token, signal, capability) = fixture();
    let embedding_calls = Arc::new(AtomicUsize::new(0));
    runtime.register_embedder(CountingProvider(Arc::clone(&embedding_calls)));
    let mut body = message();
    let root = tempfile::tempdir().unwrap();
    let content_ref = if with_attachment {
        let blobs = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0).unwrap(),
        );
        let original = blobs.put(b"quarantined original".to_vec()).await.unwrap();
        runtime.install_blob_store(blobs).unwrap();
        body["metadata"] = json!({
            "quarantined": true, "quarantine_content_ref": original.as_str(),
        });
        Some(original)
    } else {
        None
    };
    install_fts_refusal(&runtime, &token);
    let observed = signal.snapshot();
    let receipt = ingest(&runtime, &token, &signal, &capability, body.clone())
        .await
        .expect("durable ingest stays acknowledged despite indexing failure");
    let id: Uuid = receipt["full_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(receipt["full_id"], id.to_string());
    assert_eq!(receipt["id"], short_id(id));
    let thread_id: Uuid = receipt["thread_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(receipt["thread_id"], thread_id.to_string());
    assert_eq!(receipt["external_id"], body["external_id"]);
    assert_eq!(receipt["deduplicated"], false);
    let failures = receipt["post_commit_degradations"]
        .as_array()
        .expect("committed receipt must retain actual indexing diagnostics");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["stage"], "fts_upsert");
    assert!(failures[0]["error"]
        .as_str()
        .unwrap()
        .contains("comm ingest FTS refusal"));
    assert_eq!(
        signal.snapshot(),
        observed + 1,
        "committed degradation publishes exactly once"
    );
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.content, body["content"].as_str().unwrap());
    assert_eq!(
        note.properties.as_ref().unwrap()["thread_id"],
        receipt["thread_id"]
    );
    assert!(runtime
        .text_for_notes(&token)
        .unwrap()
        .get_document("local", id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(embedding_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .vectors_for_model(&token, "ingest-counting")
            .unwrap()
            .get_vectors(&[id], "local", "note.content")
            .await
            .unwrap()[&id],
        vec![1.0; 4]
    );
    if let Some(expected) = &content_ref {
        let owners = runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].role, "quarantine-original");
        assert_eq!(&owners[0].content_ref, expected);
        assert_eq!(owners[0].substrate, AttachmentSubstrate::Note);
        assert!(note.expires_at.is_some());
    }
    let replay = ingest(&runtime, &token, &signal, &capability, body)
        .await
        .unwrap();
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["thread_id"], receipt["thread_id"]);
    assert!(replay.get("post_commit_degradations").is_none());
    assert_eq!(signal.snapshot(), observed + 1);
    assert_eq!(
        embedding_calls.load(Ordering::SeqCst),
        1,
        "dedup must not reindex"
    );
    assert_eq!(
        runtime
            .list_notes(&token, Some("message"), 10, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    if let Some(expected) = content_ref {
        let owners = runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].content_ref, expected);
        let replayed = runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert!(replayed.expires_at.unwrap() >= note.expires_at.unwrap());
    }
}

#[tokio::test]
async fn ingest_committed_fts_failure_reports_and_wakes_once() {
    assert_degraded_ingest(false).await;
}

#[tokio::test]
async fn quarantine_ingest_degradation_retains_attachment_and_dedup_receipt() {
    assert_degraded_ingest(true).await;
}

#[tokio::test]
async fn healthy_ingest_omits_degradations_and_precommit_refusal_does_not_wake() {
    let (runtime, token, signal, capability) = fixture();
    let mut invalid = message();
    invalid["content"] = json!("");
    let error = ingest(&runtime, &token, &signal, &capability, invalid)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    assert_eq!(signal.snapshot(), 0);
    assert!(runtime
        .list_notes(&token, Some("message"), 10, 0)
        .await
        .unwrap()
        .is_empty());
    let receipt = ingest(&runtime, &token, &signal, &capability, message())
        .await
        .unwrap();
    assert!(receipt.get("post_commit_degradations").is_none());
    assert_eq!(receipt["deduplicated"], false);
    assert_eq!(signal.snapshot(), 1);
}

#[tokio::test]
async fn ingest_precommit_storage_failure_stays_an_error_without_a_wake() {
    let (runtime, token, signal, capability) = fixture();
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_ingest_note BEFORE INSERT ON notes \
         BEGIN SELECT RAISE(ABORT, 'precommit note refusal'); END;",
        )
        .unwrap();
    let error = ingest(&runtime, &token, &signal, &capability, message())
        .await
        .expect_err("uncommitted storage failure is not an acknowledgement");
    assert!(error.to_string().contains("precommit note refusal"));
    assert!(committed_ingest_degradations(&error).is_none());
    assert_eq!(signal.snapshot(), 0);
    assert!(runtime
        .list_notes(&token, Some("message"), 10, 0)
        .await
        .unwrap()
        .is_empty());
}

fn error_details(id: Uuid) -> Vec<(&'static str, String)> {
    vec![
        ("reason", "post_commit_degraded".into()),
        ("operation", "try_create_note".into()),
        ("committed", "true".into()),
        ("retryable", "false".into()),
        ("record_id", id.to_string()),
        (
            "post_commit_degradations",
            json!([
                {"stage": "fts_upsert", "error": "actual failed stage"},
                {"stage": "embedding", "error": "model broken: inference failure"},
            ])
            .to_string(),
        ),
    ]
}

fn committed_error(details: Vec<(&'static str, String)>) -> RuntimeError {
    khive_types::KhiveError::internal("committed ingest failure")
        .with_details(khive_types::Details::new_owned(details))
        .into()
}

#[test]
fn committed_ingest_decoder_preserves_the_exact_report() {
    let id = Uuid::new_v4();
    let mut details = error_details(id);
    details.last_mut().unwrap().1 = json!([
        {"stage": "fts_acquisition", "error": "actual FTS acquisition failure"},
        {"stage": "fts_upsert", "error": "actual FTS insertion failure"},
        {"stage": "embedding", "error": "model broken: inference failure"},
        {"stage": "vector_acquisition", "error": "model broken: storage failure"},
        {"stage": "vector_insert", "error": "model broken: publication failure"},
    ])
    .to_string();
    let expected: Value = serde_json::from_str(&details.last().unwrap().1).unwrap();
    let error = committed_error(details);
    assert_eq!(committed_ingest_degradations(&error), Some((id, expected)));
}

#[test]
fn committed_ingest_decoder_accepts_every_stage_the_runtime_can_record() {
    use khive_runtime::ConditionalInsertStage as Stage;
    // `Stage::ALL` is generated from the same declaration as the variants, so this loop
    // reaches every stage the runtime can record.
    let labels: std::collections::BTreeSet<_> = Stage::ALL.iter().map(|s| s.label()).collect();
    assert_eq!(
        labels.len(),
        Stage::ALL.len(),
        "stage labels must be distinct"
    );
    for &stage in Stage::ALL {
        assert_eq!(Stage::from_label(stage.label()), Some(stage));
        let id = Uuid::new_v4();
        let mut details = error_details(id);
        details.last_mut().unwrap().1 =
            json!([{"stage": stage.label(), "error": "stage failure"}]).to_string();
        assert!(
            committed_ingest_degradations(&committed_error(details)).is_some(),
            "the decoder must accept the runtime stage {stage:?}"
        );
    }
}

#[test]
fn committed_ingest_decoder_rejects_unrelated_and_malformed_errors() {
    let id = Uuid::parse_str("12345678-abcd-4234-8234-123456789abc").unwrap();
    assert!(
        committed_ingest_degradations(&RuntimeError::Internal("post_commit_degraded".into()))
            .is_none()
    );
    let wrong_kind: RuntimeError =
        khive_types::KhiveError::invalid_input("same details, wrong kind")
            .with_details(khive_types::Details::new_owned(error_details(id)))
            .into();
    assert!(committed_ingest_degradations(&wrong_kind).is_none());
    assert!(committed_ingest_degradations(
        &khive_types::KhiveError::internal("missing details").into()
    )
    .is_none());
    for missing in [
        "reason",
        "operation",
        "committed",
        "retryable",
        "record_id",
        "post_commit_degradations",
    ] {
        let details = error_details(id)
            .into_iter()
            .filter(|(key, _)| *key != missing)
            .collect();
        assert!(
            committed_ingest_degradations(&committed_error(details)).is_none(),
            "missing {missing}"
        );
    }
    for (key, value) in [
        ("reason", "embedding_input_truncated".to_string()),
        ("operation", "delete_note".to_string()),
        ("committed", "false".to_string()),
        ("retryable", "true".to_string()),
        ("record_id", "12345678".to_string()),
        ("record_id", id.to_string().to_uppercase()),
        ("record_id", id.simple().to_string()),
        ("post_commit_degradations", "not json".to_string()),
        ("post_commit_degradations", "null".to_string()),
        ("post_commit_degradations", "{}".to_string()),
        ("post_commit_degradations", "[]".to_string()),
        (
            "post_commit_degradations",
            r#"[{"stage":"fts_upsert"}]"#.to_string(),
        ),
        (
            "post_commit_degradations",
            r#"[{"stage":"fts_upsert","error":false}]"#.to_string(),
        ),
        (
            "post_commit_degradations",
            r#"[{"stage":"fts_upsert","error":""}]"#.to_string(),
        ),
        (
            "post_commit_degradations",
            r#"[{"stage":"future_unknown_stage","error":"failure"}]"#.to_string(),
        ),
        (
            "post_commit_degradations",
            r#"[{"stage":"fts_upsert","error":"failure","extra":true}]"#.to_string(),
        ),
        (
            "post_commit_degradations",
            r#"[{"stage":"fts_upsert","error":"failure"},null]"#.to_string(),
        ),
    ] {
        let mut details = error_details(id);
        details.iter_mut().find(|(name, _)| *name == key).unwrap().1 = value.clone();
        let error = committed_error(details);
        assert!(
            committed_ingest_degradations(&error).is_none(),
            "must reject {key}={value}"
        );
    }
}
