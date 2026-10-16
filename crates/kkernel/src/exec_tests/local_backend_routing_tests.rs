// ── build_local_fallback_server multi-backend routing (D1-R2) ────────────
//
// Before this fix, both of exec's local-dispatch call sites always built a
// single-backend runtime pointed at `cfg.db_path`, regardless of any
// `[[backends]]` declaration in `khive_cfg`. A config pinning a pack (e.g.
// `comm`) to a separate backend would have that pack's writes silently
// land in whatever single file `cfg.db_path` pointed at instead of the
// declared backend file. This test pins `comm` to a second, file-backed
// `secondary` backend and proves the write lands there — not in `main` —
// by re-opening each backend file independently afterward.

/// D1-R2 regression proof: `build_local_fallback_server` must delegate to
/// `build_server_multi_backend` (not the single-backend `KhiveMcpServer::new`)
/// whenever `khive_cfg.backends` is non-empty, and pack routing must actually
/// take effect end to end.
#[tokio::test]
#[serial]
async fn build_local_fallback_server_routes_through_multi_backend_when_backends_declared() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_runtime::{BackendConfig, BackendKind, PackConfig};

    let lock_dir = tempfile::tempdir().expect("private construction lock");
    let _env = EnvAndCwdGuard::capture();
    std::env::set_var("KHIVE_LOCK", lock_dir.path().join("khived.recovery.lock"));

    let main_db = NamedTempFile::new().expect("main db tempfile");
    let secondary_db = NamedTempFile::new().expect("secondary db tempfile");
    let main_path = main_db.path().to_path_buf();
    let secondary_path = secondary_db.path().to_path_buf();

    let khive_cfg = KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
            BackendConfig {
                name: "secondary".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(secondary_path.clone()),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
            },
        ],
        packs: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "comm".to_string(),
                PackConfig {
                    backend: "secondary".to_string(),
                    verbs_disabled: Vec::new(),
                    no_embed: false,
                },
            );
            m
        },
        ..KhiveConfig::default()
    };

    // `db_path` here is NOT the actual storage location when `[[backends]]`
    // is declared — `build_server_multi_backend` opens each backend's own
    // declared path (the tempfiles above) independently. It is only the
    // identity/fingerprint value `assert_captured_db_anchor_consistent` checks
    // against `resolve_db_anchor(cli_db_override)`, exactly mirroring what
    // a real `kkernel exec` invocation with NO explicit `--db` flag would
    // resolve to (the realistic shape when `[[backends]]` fully governs
    // storage) — see `base_runtime_config_for_multi_backend` in serve.rs's
    // own multi-backend test suite for the identical pattern.
    let cfg = RuntimeConfig {
        db_path: khive_runtime::resolve_db_anchor(None),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        actor_id: Some("actor-routing-test".to_string()),
        ..RuntimeConfig::default()
    };

    // No explicit `--db` override — `[[backends]]` alone governs storage,
    // matching the `cfg.db_path` shape above. An explicit override here
    // would be rejected as ambiguous by `build_registry_for_multi_backend`
    // (ADR-028 §8) since 2 backends are already declared.
    let db_anchor = cfg.db_path.clone();
    let server = build_local_fallback_server(cfg, &khive_cfg, None, db_anchor.as_deref())
        .await
        .expect("multi-backend local fallback must build");
    assert!(lock_dir.path().join("khived.recovery.lock").is_file());

    let send = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: r#"comm.send(to="actor-routing-test", content="routed-via-secondary", self_send=true)"#
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("comm.send must dispatch");
    let send_resp: serde_json::Value = serde_json::from_str(&send).expect("valid JSON");
    assert_eq!(
        send_resp["results"][0]["ok"].as_bool(),
        Some(true),
        "comm.send must succeed through the multi-backend fallback server: {send_resp}"
    );

    // Inspect each physical file directly: list routing and mailbox
    // filtering must not influence the backend-placement assertion.
    fn count_messages(db_path: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read-only connection to backend file");
        conn.query_row(
            "SELECT COUNT(*) FROM notes WHERE kind = 'message' AND content = ?1",
            ["routed-via-secondary"],
            |row| row.get(0),
        )
        .expect("count persisted message notes")
    }

    let main_count = count_messages(&main_path);
    let secondary_count = count_messages(&secondary_path);

    assert_eq!(
        main_count, 0,
        "comm pack must NOT write into the `main` backend file when pinned to \
             `secondary` (D1-R2: a silent single-backend fallback would have written \
             it here instead)"
    );
    assert_eq!(
        secondary_count, 2,
        "comm pack write must land in its declared `secondary` backend file — \
             `comm.send` dual-writes an outbound + inbound note copy per message \
             (khive-pack-comm's message.rs), both via the SAME pack runtime, so a \
             single self-send yields 2 `message` notes in whichever backend `comm` \
             is pinned to"
    );
}

#[tokio::test]
#[serial]
async fn build_local_fallback_server_uses_captured_anchor_after_home_changes() {
    if crate::test_process::run_in_child() {
        return;
    }

    let (previous_home, _first_home) = isolate_home_for_test();
    let cfg = RuntimeConfig {
        db_path: khive_runtime::resolve_db_anchor(None),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let db_anchor = cfg.db_path.clone();
    let khive_cfg = KhiveConfig {
        backends: vec![khive_runtime::BackendConfig {
            name: "main".to_string(),
            kind: khive_runtime::BackendKind::Memory,
            path: None,
            cache_mb: None,
            journal_mode: None,
            served_kinds: None,
            read_only: false,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
        }],
        ..KhiveConfig::default()
    };
    let second_home = tempfile::tempdir().expect("second HOME");
    std::env::set_var("HOME", second_home.path());

    let result = build_local_fallback_server(cfg, &khive_cfg, None, db_anchor.as_deref()).await;
    restore_home(previous_home);

    assert!(
        result.is_ok(),
        "exec fallback must use the anchor captured with RuntimeConfig after HOME changes: {}",
        result.err().unwrap()
    );
}

// ── single-backend fallback installs a BlobStore (khive#1209) ────────────
//
// Before this fix, `build_local_fallback_server`'s single-backend branch
// constructed `KhiveRuntime`/`KhiveMcpServer` without ever calling
// `install_resolved_blob_store`, so `blob.*` verbs dispatched through
// `kkernel exec`'s in-process fallback always saw an unconfigured
// `BlobStore` even when the same config/backend combination resolves one
// for the `serve` daemon boot path. `KhiveMcpServer` does not expose its
// wrapped runtime, so this asserts the same *observable* side effect the
// `serve` path's own tests rely on: `FsBlobStore::new` (khive-db
// `stores/blob.rs`) creates its root directory eagerly. With no
// `[storage.blob]` config and no `KHIVE_BLOB_ROOT`, resolution falls
// back to `<db_dir>/blobs` — that directory existing after construction
// is proof the install call ran.
#[tokio::test]
#[serial]
async fn build_local_fallback_server_installs_blob_store_single_backend() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let _env = EnvAndCwdGuard::capture();
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let db_path = dir.path().join("exec_blob.db");
    let cfg = RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: RuntimeConfig::built_in_packs(),
        ..RuntimeConfig::default()
    };
    let khive_cfg = KhiveConfig::default();

    let _server = build_local_fallback_server(cfg, &khive_cfg, None, None)
        .await
        .expect("single-backend local-exec construction must succeed");
    assert!(dir.path().join("khived.recovery.lock").is_file());

    assert!(
        dir.path().join("blobs").is_dir(),
        "default <db_dir>/blobs root must exist after construction, proving \
             install_resolved_blob_store ran for the single-backend fallback path"
    );
}
