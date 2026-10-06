/// Physical isolation guard: a record written through a pack pinned to backend B's
/// SQLite file MUST NOT appear in backend A's file, and vice versa.
///
/// This is the "billing data must not mix with agent memory" guarantee.
/// The test opens each file independently with rusqlite after the server is
/// dropped to confirm cross-file absence in both directions.
///
/// Schema facts discovered from crates/khive-db/sql/:
///   entities table — column `name` holds the entity name (entities-ddl.sql)
///   notes table    — column `content` holds the message body; `kind` = "message"
///                    for comm.send output (notes-ddl.sql + comm handlers.rs)
///
/// Relies on `base_runtime_config_for_multi_backend` leaving `embedding_model`
/// unset: no embedder means no `vec0` virtual table is created, so the plain
/// `rusqlite::Connection::open` below (which does not load the vec0 extension)
/// can read both files. If an embedder is ever added to that helper, this test
/// must load the extension or query through a runtime instead.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn multi_backend_isolates_pack_data_to_separate_files() {
    use crate::tools::request::RequestParams;
    use khive_runtime::PackConfig;
    use rusqlite::Connection;

    let dir = tempfile::tempdir().expect("temp dir");
    let main_path = dir.path().join("main.db");
    let second_path = dir.path().join("second.db");

    let khive_cfg = KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(main_path.clone()),
                cache_mb: None,
                journal_mode: None,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
                served_kinds: None,
                read_only: false,
            },
            BackendConfig {
                name: "second".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(second_path.clone()),
                cache_mb: None,
                journal_mode: None,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
                served_kinds: None,
                read_only: false,
            },
        ],
        packs: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "comm".to_string(),
                PackConfig {
                    backend: "second".to_string(),
                    no_embed: false,
                },
            );
            m
        },
        ..KhiveConfig::default()
    };

    let base_cfg = base_runtime_config_for_multi_backend();

    let server = build_server_multi_backend(base_cfg, &khive_cfg, None)
        .await
        .expect("multi-backend boot must succeed");

    let dispatch = |ops: String| {
        let server = &server;
        async move {
            server
                .dispatch_request_local(RequestParams {
                    plan: None,
                    ops,
                    presentation: None,
                    presentation_per_op: None,
                    save_to: None,
                    format: None,
                    format_per_op: None,
                    request_id: None,
                })
                .await
                .expect("dispatch must not error")
        }
    };

    // kg → main.db: create an entity
    let kg_resp = dispatch(r#"create(kind="concept", name="MainOnlyEntity")"#.to_string()).await;
    let kg_json: serde_json::Value =
        serde_json::from_str(&kg_resp).expect("kg response is valid JSON");
    assert_eq!(
        kg_json["results"][0]["ok"].as_bool(),
        Some(true),
        "kg create must succeed; response: {kg_resp}"
    );

    // comm → second.db: send a message
    let comm_resp = dispatch(r#"comm.send(to="local", content="SecondOnlyMsg")"#.to_string()).await;
    let comm_json: serde_json::Value =
        serde_json::from_str(&comm_resp).expect("comm response is valid JSON");
    assert_eq!(
        comm_json["results"][0]["ok"].as_bool(),
        Some(true),
        "comm.send must succeed; response: {comm_resp}"
    );

    // Drop the server so WAL is checkpointed and files are fully flushed
    // before we open them with rusqlite.
    drop(server);

    // --- Verify main.db ---
    let main_conn = Connection::open(&main_path).expect("open main.db");

    let main_entity_count: i64 = main_conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE name = 'MainOnlyEntity' \
                 AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("query entities in main.db");
    assert_eq!(
        main_entity_count, 1,
        "main.db MUST contain MainOnlyEntity (written via kg pack); \
             got count={main_entity_count}"
    );

    let main_msg_count: i64 = main_conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE kind = 'message'",
            [],
            |row| row.get(0),
        )
        .expect("query notes in main.db");
    assert_eq!(
        main_msg_count, 0,
        "main.db MUST NOT contain any message notes (comm is pinned to second.db); \
             got count={main_msg_count}"
    );

    // --- Verify second.db ---
    let second_conn = Connection::open(&second_path).expect("open second.db");

    let second_msg_count: i64 = second_conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE kind = 'message' AND content = 'SecondOnlyMsg'",
            [],
            |row| row.get(0),
        )
        .expect("query notes in second.db");
    assert_eq!(
        second_msg_count, 2,
        "second.db MUST contain SecondOnlyMsg (dual-write: 1 outbound + 1 inbound copy); \
             got count={second_msg_count}"
    );

    let second_entity_count: i64 = second_conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE name = 'MainOnlyEntity'",
            [],
            |row| row.get(0),
        )
        .expect("query entities in second.db");
    assert_eq!(
        second_entity_count, 0,
        "second.db MUST NOT contain MainOnlyEntity (kg is pinned to main.db); \
             got count={second_entity_count}"
    );
}

/// Tilde divergence regression (blocking security defect): a `~/`-prefixed
/// `--db`/`KHIVE_DB` override must fingerprint identically to the
/// equivalent absolute path. Before the fix, `resolve_db_anchor` left a
/// leading `~` unexpanded in `RuntimeConfig.db_path`, so single-backend
/// boot opened a literal `./~/...` file while `compute_config_id`
/// canonicalized (and so expanded) the same raw string separately —
/// letting two processes that open different files still collide on one
/// `config_id` and get dispatched to each other's database by the daemon.
#[test]
#[serial]
fn config_id_matches_for_tilde_and_equivalent_absolute_db_override() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let original_home = std::env::var_os("HOME");
    let home_dir = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("HOME", home_dir.path());

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let tilde_anchor = khive_runtime::resolve_db_anchor(Some("~/data.db"))
            .expect("an explicit path always anchors");
        let expected = home_dir.path().join("data.db");
        assert_eq!(
            tilde_anchor, expected,
            "resolve_db_anchor must expand a leading ~ before compute_config_id ever \
                 sees it"
        );

        let absolute_anchor =
            khive_runtime::resolve_db_anchor(Some(expected.to_str().expect("utf8 tempdir path")))
                .expect("an explicit path always anchors");
        assert_eq!(tilde_anchor, absolute_anchor);

        let make_config = |db_path: std::path::PathBuf| RuntimeConfig {
            db_path: Some(db_path),
            default_namespace: khive_runtime::Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::no_embeddings()
        };

        let tilde_cfg = make_config(tilde_anchor);
        let abs_cfg = make_config(absolute_anchor);

        let id_tilde = crate::server::compute_config_id(&tilde_cfg, None);
        let id_abs = crate::server::compute_config_id(&abs_cfg, None);
        assert_eq!(
            id_tilde, id_abs,
            "a config resolved from a ~-prefixed override must fingerprint identically \
                 to one resolved from the equivalent absolute path"
        );
    }));

    match original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    outcome.expect("test body panicked");
}
