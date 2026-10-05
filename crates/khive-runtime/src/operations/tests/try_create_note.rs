use super::*;

#[tokio::test]
async fn try_create_note_rejects_reserved_secret_gate_key() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let err = rt
        .try_create_note(
            &tok,
            "observation",
            None,
            "reserved-key conditional note",
            Some(reserved_key_props()),
        )
        .await
        .expect_err("caller-supplied reserved key must be rejected");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("khive:secret_gate")),
        "unexpected error: {err:?}"
    );
}

/// One value for each key of the `message` entry of the kind-owned property
/// list, shaped like what the owning transport writes.
fn transport_owned_message_properties() -> [(&'static str, serde_json::Value); 7] {
    use serde_json::json;

    [
        ("quarantined", json!(true)),
        ("channel_kind", json!("email")),
        ("channel_slug", json!("forged-channel")),
        ("delivery_hold", json!("external_id_unverifiable")),
        ("delivery_hold_reason", json!("forged hold")),
        ("delivery_hold_at", json!("2026-01-01T00:00:00Z")),
        ("external_id_diagnostic_note_id", json!("diag-note")),
    ]
}

#[tokio::test]
async fn try_create_note_refuses_every_transport_owned_message_property() {
    let rt = rt();
    let tok = NamespaceToken::local();

    for (key, value) in transport_owned_message_properties() {
        let err = rt
            .try_create_note(
                &tok,
                "message",
                None,
                "forged transport-owned property via direct runtime write",
                Some(serde_json::json!({ key: value })),
            )
            .await
            .expect_err(&format!("try_create_note must refuse `{key}`"));
        assert!(
            matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains(key)),
            "refusal must name `{key}`: {err:?}"
        );
    }

    let left_behind = rt
        .list_notes(&tok, Some("message"), 100, 0)
        .await
        .expect("list must succeed");
    assert!(
        left_behind.is_empty(),
        "refused writes must leave no row behind: {left_behind:?}"
    );

    // Control: the same read sees a message row, and the refusal is key-scoped.
    let created = rt
        .try_create_note(
            &tok,
            "message",
            None,
            "ordinary message",
            Some(serde_json::json!({"direction": "inbound"})),
        )
        .await
        .expect("a message without transport-owned properties is accepted")
        .expect("the insert is not deduplicated");
    let rows = rt
        .list_notes(&tok, Some("message"), 100, 0)
        .await
        .expect("list must succeed");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, created.id);
}

#[tokio::test]
async fn trusted_ingest_accepts_every_transport_owned_message_property() {
    let rt = rt();
    let tok = NamespaceToken::local();
    let capability = crate::pack::ChannelIngestCapability { _sealed: () };

    let mut written = Vec::new();
    for (key, value) in transport_owned_message_properties() {
        let note = rt
            .try_create_note_as_trusted_ingest(
                &capability,
                &tok,
                "message",
                None,
                "trusted ingest establishes a transport-owned property",
                Some(serde_json::json!({ key: value })),
                None,
            )
            .await
            .unwrap_or_else(|err| panic!("trusted ingest refused `{key}`: {err}"))
            .expect("the insert is not deduplicated");
        written.push((key, value, note.id));
    }

    let mut everything = serde_json::Map::new();
    for (key, value) in transport_owned_message_properties() {
        everything.insert(key.to_string(), value);
    }
    let all_at_once = rt
        .try_create_note_as_trusted_ingest(
            &capability,
            &tok,
            "message",
            None,
            "trusted ingest establishes every transport-owned property",
            Some(serde_json::Value::Object(everything)),
            None,
        )
        .await
        .expect("trusted ingest must accept all transport-owned properties at once")
        .expect("the insert is not deduplicated");

    let rows = rt
        .list_notes(&tok, Some("message"), 100, 0)
        .await
        .expect("list must succeed");
    assert_eq!(rows.len(), written.len() + 1);
    for (key, value, id) in written {
        let row = rows
            .iter()
            .find(|note| note.id == id)
            .expect("the trusted ingest row is persisted");
        let props = row.properties.as_ref().expect("properties");
        assert_eq!(props[key], value, "must persist `{key}`");
    }
    let stored = rows
        .iter()
        .find(|note| note.id == all_at_once.id)
        .and_then(|note| note.properties.as_ref())
        .and_then(serde_json::Value::as_object)
        .expect("the all-at-once row keeps its properties");
    assert_eq!(stored.len(), 7);
}

fn install_fts_insert_refusal(runtime: &KhiveRuntime, token: &NamespaceToken) {
    runtime.text_for_notes(token).expect("initialize FTS map");
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_ingest_fts BEFORE INSERT ON fts_notes_rowids \
         BEGIN SELECT RAISE(ABORT, 'ingest FTS refusal'); END;",
        )
        .expect("install actual FTS map refusal");
}

fn committed_failure(error: RuntimeError) -> (Uuid, serde_json::Value) {
    let RuntimeError::Khive(domain) = error.refusal_source() else {
        panic!("expected typed committed error: {error:?}");
    };
    assert_eq!(domain.kind(), khive_types::ErrorKind::Internal);
    let details = domain.details().unwrap();
    assert_eq!(details.get("reason"), Some("post_commit_degraded"));
    assert_eq!(details.get("operation"), Some("try_create_note"));
    assert_eq!(details.get("committed"), Some("true"));
    assert_eq!(details.get("retryable"), Some("false"));
    let raw_id = details.get("record_id").unwrap();
    let id: Uuid = raw_id.parse().unwrap();
    assert_eq!(id.to_string(), raw_id);
    let failures: serde_json::Value =
        serde_json::from_str(details.get("post_commit_degradations").unwrap()).unwrap();
    assert!(!failures.as_array().unwrap().is_empty());
    let projected =
        crate::error_projection::runtime_error_value(error, crate::DomainDisposition::Unknown);
    assert_eq!(projected["domain_disposition"], "committed");
    (id, failures)
}

async fn assert_committed_note(runtime: &KhiveRuntime, token: &NamespaceToken, id: Uuid) {
    let stored = runtime
        .notes(token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "conditional ingest disclosure");
    assert_eq!(stored.id, id);
    assert_eq!(
        runtime.list_notes(token, None, 10, 0).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn conditional_ingest_reports_fts_failure_after_durable_insert() {
    let runtime = rt();
    let token = NamespaceToken::local();
    install_fts_insert_refusal(&runtime, &token);
    let error = runtime
        .try_create_note(
            &token,
            "observation",
            None,
            "conditional ingest disclosure",
            None,
        )
        .await
        .expect_err("committed FTS failure must be visible");
    let (id, failures) = committed_failure(error);
    assert_committed_note(&runtime, &token, id).await;
    assert_eq!(failures.as_array().unwrap().len(), 1);
    assert_eq!(failures[0]["stage"], "fts_upsert");
    assert!(failures[0]["error"]
        .as_str()
        .unwrap()
        .contains("ingest FTS refusal"));
    assert!(runtime
        .text_for_notes(&token)
        .unwrap()
        .get_document("local", id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn trusted_ingest_reports_committed_fts_failure() {
    let runtime = rt();
    let token = NamespaceToken::local();
    let capability = crate::pack::ChannelIngestCapability { _sealed: () };
    install_fts_insert_refusal(&runtime, &token);
    let error = runtime
        .try_create_note_as_trusted_ingest(
            &capability,
            &token,
            "message",
            None,
            "conditional ingest disclosure",
            Some(serde_json::json!({"channel_kind": "email", "quarantined": true})),
            None,
        )
        .await
        .expect_err("trusted caller must receive committed failure");
    let (id, failures) = committed_failure(error);
    assert_committed_note(&runtime, &token, id).await;
    assert_eq!(failures[0]["stage"], "fts_upsert");
    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.properties.unwrap()["quarantined"], true);
}

#[tokio::test]
async fn trusted_attachment_ingest_failure_retains_original_owner() {
    use khive_storage::BlobStore as _;

    let runtime = rt();
    let token = NamespaceToken::local();
    let capability = crate::pack::ChannelIngestCapability { _sealed: () };
    let root = tempfile::tempdir().unwrap();
    let blobs =
        Arc::new(khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0).unwrap());
    let content_ref = blobs.put(b"ingest original".to_vec()).await.unwrap();
    runtime.install_blob_store(blobs).unwrap();
    install_fts_insert_refusal(&runtime, &token);
    let error = runtime
        .try_create_note_as_trusted_ingest_with_attachment(
            &capability,
            &token,
            "message",
            None,
            "conditional ingest disclosure",
            Some(serde_json::json!({"quarantined": true})),
            NewAttachment {
                role: "quarantine-original".into(),
                content_ref: content_ref.clone(),
                media_type: None,
                size_bytes: None,
            },
            Some(std::time::Duration::from_secs(60)),
        )
        .await
        .expect_err("attachment note is committed with visible indexing failure");
    let (id, failures) = committed_failure(error);
    assert_committed_note(&runtime, &token, id).await;
    assert_eq!(failures[0]["stage"], "fts_upsert");
    let owners = runtime
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].role, "quarantine-original");
    assert_eq!(owners[0].content_ref, content_ref);
    assert_eq!(owners[0].substrate, AttachmentSubstrate::Note);
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
        .expires_at
        .is_some());
}

#[derive(Clone, Copy)]
enum ModelFault {
    Provider,
    Store,
    Publication,
}

async fn assert_model_failure(fault: ModelFault, expected_stage: &str) {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let runtime = rt();
    let token = NamespaceToken::local();
    let mut build_counts = std::collections::HashMap::new();
    for name in ["model_a", "model_b"] {
        let (provider, builds) = ConstVecProvider::new(name, 4);
        runtime.register_embedder(provider);
        build_counts.insert(name.to_string(), builds);
    }
    // Registry iteration is unspecified; fail its actual first model so a
    // fail-fast regression cannot pass by visiting the healthy model first.
    let model_order = runtime.embedding_models_for_note_kind("observation");
    assert_eq!(model_order.len(), 2);
    let broken_model = &model_order[0];
    let healthy_model = &model_order[1];
    runtime.vectors_for_model(&token, healthy_model).unwrap();
    if matches!(fault, ModelFault::Provider) {
        runtime.register_embedder(FailFastProvider::new(broken_model));
        install_fts_insert_refusal(&runtime, &token);
    } else if matches!(fault, ModelFault::Publication) {
        runtime.vectors_for_model(&token, broken_model).unwrap();
    }
    assert_eq!(
        runtime.embedding_models_for_note_kind("observation"),
        model_order
    );
    let sql_faults = Arc::new(AtomicUsize::new(0));
    if !matches!(fault, ModelFault::Provider) {
        let observed = Arc::clone(&sql_faults);
        let table = format!("vec_{broken_model}");
        runtime
            .backend()
            .pool()
            .writer()
            .unwrap()
            .conn()
            .authorizer(Some(move |context: AuthContext<'_>| {
                let deny = match fault {
                    ModelFault::Store => matches!(context.action,
                        AuthAction::CreateVtable { table_name, .. } if table_name == table),
                    ModelFault::Publication => matches!(context.action,
                        AuthAction::Insert { table_name } if table_name == table),
                    ModelFault::Provider => false,
                };
                if deny {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
    }
    let result = runtime
        .try_create_note(
            &token,
            "observation",
            None,
            "conditional ingest disclosure",
            None,
        )
        .await;
    if !matches!(fault, ModelFault::Provider) {
        runtime
            .backend()
            .pool()
            .writer()
            .unwrap()
            .conn()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert!(
            sql_faults.load(Ordering::SeqCst) > 0,
            "real vector SQL refusal must execute"
        );
    }
    let (id, failures) = committed_failure(result.expect_err("failed model must be visible"));
    assert_committed_note(&runtime, &token, id).await;
    let failures = failures.as_array().unwrap();
    assert_eq!(
        failures.len(),
        if matches!(fault, ModelFault::Provider) {
            2
        } else {
            1
        }
    );
    let failure = failures
        .iter()
        .find(|entry| entry["stage"] == expected_stage)
        .expect("specific failed model stage");
    assert!(failure["error"]
        .as_str()
        .unwrap()
        .contains(&format!("model {broken_model}:")));
    assert_eq!(build_counts[healthy_model].load(Ordering::SeqCst), 1);
    let vectors = runtime
        .vectors_for_model(&token, healthy_model)
        .unwrap()
        .get_vectors(&[id], "local", "note.content")
        .await
        .unwrap();
    assert_eq!(vectors.get(&id), Some(&vec![1.0; 4]));
    assert!(runtime
        .vectors_for_model(&token, broken_model)
        .unwrap()
        .get_vectors(&[id], "local", "note.content")
        .await
        .unwrap()
        .is_empty());
    if matches!(fault, ModelFault::Provider) {
        assert_eq!(failures[0]["stage"], "fts_upsert");
        assert!(failures[0]["error"]
            .as_str()
            .unwrap()
            .contains("ingest FTS refusal"));
    } else {
        assert_eq!(
            runtime
                .text_for_notes(&token)
                .unwrap()
                .get_document("local", id)
                .await
                .unwrap()
                .unwrap()
                .body,
            "conditional ingest disclosure"
        );
    }
}

#[tokio::test]
async fn conditional_ingest_retains_fts_and_model_failures_with_healthy_vectors() {
    assert_model_failure(ModelFault::Provider, "embedding").await;
}

#[tokio::test]
async fn conditional_ingest_reports_vector_acquisition_failure() {
    assert_model_failure(ModelFault::Store, "vector_acquisition").await;
}

#[tokio::test]
async fn conditional_ingest_reports_vector_publication_failure() {
    assert_model_failure(ModelFault::Publication, "vector_insert").await;
}

#[tokio::test]
async fn conditional_ingest_reports_fts_acquisition_failure() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let runtime = rt();
    let token = NamespaceToken::local();
    runtime.text_for_notes(&token).unwrap();
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute_batch("DROP TABLE fts_notes")
        .unwrap();
    let faults = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&faults);
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::CreateVtable {
                    table_name: "fts_notes",
                    ..
                }
            ) {
                observed.fetch_add(1, Ordering::SeqCst);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let result = runtime
        .try_create_note(
            &token,
            "observation",
            None,
            "conditional ingest disclosure",
            None,
        )
        .await;
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(faults.load(Ordering::SeqCst) > 0);
    let (id, failures) = committed_failure(result.expect_err("FTS acquisition failure is visible"));
    assert_committed_note(&runtime, &token, id).await;
    assert_eq!(failures.as_array().unwrap().len(), 1);
    assert_eq!(failures[0]["stage"], "fts_acquisition");
}

#[tokio::test]
async fn conditional_ingest_healthy_insert_keeps_note_and_index_success() {
    let runtime = rt();
    let token = NamespaceToken::local();
    let (provider, _) = ConstVecProvider::new("healthy", 4);
    runtime.register_embedder(provider);
    let note = runtime
        .try_create_note(
            &token,
            "observation",
            None,
            "conditional ingest disclosure",
            None,
        )
        .await
        .unwrap()
        .expect("fresh insert");
    assert_committed_note(&runtime, &token, note.id).await;
    assert_eq!(
        runtime
            .text_for_notes(&token)
            .unwrap()
            .get_document("local", note.id)
            .await
            .unwrap()
            .unwrap()
            .body,
        note.content
    );
    assert_eq!(
        runtime
            .vectors_for_model(&token, "healthy")
            .unwrap()
            .get_vectors(&[note.id], "local", "note.content")
            .await
            .unwrap()[&note.id],
        vec![1.0; 4]
    );
}
