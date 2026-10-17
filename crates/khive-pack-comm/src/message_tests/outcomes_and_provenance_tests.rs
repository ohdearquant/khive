#[test]
fn side_effects_unknown_surfaces_outbound_confirmation_id() {
    for with_evidence in [false, true] {
        let outbound_id = Uuid::new_v4();
        let error =
            StorageError::writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown);
        let error = if with_evidence {
            error.with_sqlite_write_failure(khive_storage::error::SqliteWriteFailure {
                stage: khive_storage::error::SqliteWriteStage::Commit,
                primary_code: 10,
                extended_code: 1034,
                settlement_unknown: false,
            })
        } else {
            error
        };
        let error = RuntimeError::Storage(error);

        let annotated = attach_outbound_id_to_ambiguous_write(outbound_id, error);
        let RuntimeError::Khive(khive_error) = &annotated else {
            panic!("expected RuntimeError::Khive, got {annotated:?}");
        };
        assert_eq!(khive_error.kind(), khive_types::ErrorKind::Conflict);
        assert_eq!(khive_error.retry_hint(), khive_types::RetryHint::NoRetry);
        assert_eq!(
            khive_error
                .details()
                .and_then(|d| d.get("outbound_id"))
                .map(str::to_string),
            Some(outbound_id.to_string())
        );
        assert!(khive_error.message().contains("side_effects_unknown"));
        assert!(khive_error
            .message()
            .contains(&format!("comm.delivered(id=\"{outbound_id}\")")));
    }
}

#[test]
fn known_atomic_rollback_is_not_mislabeled_ambiguous() {
    let error = RuntimeError::Internal("atomic multi-note write rolled back at op 1".into());
    let annotated = attach_outbound_id_to_ambiguous_write(Uuid::new_v4(), error);

    assert!(matches!(&annotated, RuntimeError::Internal(_)));
    assert!(!annotated.to_string().contains("outbound_id="));
}

#[test]
fn writer_begin_contention_is_retryable_without_delivery_probe() {
    let error = RuntimeError::Storage(StorageError::WriterTaskBusy { timeout_ms: 175 });
    let annotated = attach_outbound_id_to_ambiguous_write(Uuid::new_v4(), error);

    assert!(matches!(
        annotated,
        RuntimeError::Storage(StorageError::WriterTaskBusy { timeout_ms: 175 })
    ));
}

#[tokio::test]
async fn dual_write_versions_both_copies_and_stamps_optional_process_provenance() {
    use khive_runtime::{AllowAllGate, BackendId, RuntimeConfig};

    let runtime = KhiveRuntime::new(RuntimeConfig {
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
        default_namespace: Namespace::local(),
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
    let caller_token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");

    dual_write_message(
        &runtime,
        &caller_token,
        "local",
        "local",
        None,
        "with process provenance",
        None,
        "2026-07-31T00:00:00Z",
        Some("worker/run:42"),
        Some("local"),
        Some("local"),
        None,
        None,
        None,
    )
    .await
    .expect("dual write with provenance");
    dual_write_message(
        &runtime,
        &caller_token,
        "local",
        "local",
        None,
        "without process provenance",
        None,
        "2026-07-31T00:00:01Z",
        None,
        Some("local"),
        Some("local"),
        None,
        None,
        None,
    )
    .await
    .expect("dual write without provenance");

    let notes = runtime
        .list_notes(&caller_token, Some("message"), 100, 0)
        .await
        .expect("list messages");
    for (content, expected_process) in [
        ("with process provenance", Some("worker/run:42")),
        ("without process provenance", None),
    ] {
        let matching: Vec<_> = notes
            .iter()
            .filter(|note| note.content == content)
            .collect();
        assert_eq!(matching.len(), 2, "one outbound and one inbound copy");

        let mut directions = Vec::new();
        for note in matching {
            let props = note.properties.as_ref().expect("message properties");
            assert_eq!(
                props.get("comm_schema_version").and_then(Value::as_u64),
                Some(COMM_SCHEMA_VERSION)
            );
            assert_eq!(
                props.get("sent_by_process").and_then(Value::as_str),
                expected_process
            );
            directions.push(
                props
                    .get("direction")
                    .and_then(Value::as_str)
                    .expect("direction"),
            );
        }
        directions.sort_unstable();
        assert_eq!(directions, ["inbound", "outbound"]);
    }
}
