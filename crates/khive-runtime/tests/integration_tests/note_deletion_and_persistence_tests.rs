/// Soft-deleted notes must not appear in list_notes results.
#[tokio::test]
async fn list_notes_excludes_soft_deleted() {
    use khive_storage::types::DeleteMode;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "soft-delete-test",
            Some(0.9),
            None,
            vec![],
        )
        .await
        .unwrap();

    let notes_before = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert!(
        notes_before.iter().any(|n| n.id == note.id),
        "note should appear before soft-delete"
    );

    rt.notes(&tok)
        .unwrap()
        .delete_note(note.id, DeleteMode::Soft)
        .await
        .unwrap();

    let notes_after = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert!(
        !notes_after.iter().any(|n| n.id == note.id),
        "soft-deleted note must not appear in list"
    );
}

// =============================================================================
// File-backed runtime
// =============================================================================

#[tokio::test]
async fn file_backed_runtime_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.db");

    {
        let config = RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(path.clone()),
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            additional_embedding_models: vec![],
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        };
        let rt = KhiveRuntime::new_for_test(config).unwrap();
        let tok = rt.authorize(Namespace::local()).unwrap();
        rt.create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "Persistent",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    }

    // Re-open the same file
    {
        let config = RuntimeConfig {
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: chrono_tz::Tz::UTC,
            events_split: None,
            db_path: Some(path.clone()),
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            additional_embedding_models: vec![],
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        };
        let rt = KhiveRuntime::new_for_test(config).unwrap();
        let tok = rt.authorize(Namespace::local()).unwrap();
        let entities = rt.list_entities(&tok, None, None, 50, 0).await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "Persistent");
    }
}
