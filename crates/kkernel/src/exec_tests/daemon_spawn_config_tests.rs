// ── D1-R3 (end-to-end): exec frame config_id vs. daemon config_id ────────
//
// `exec_config_id_matches_serve_config_id_for_multi_backend_topology` above
// proves `compute_config_id` folds the topology identically for exec-shaped
// and serve-shaped `RuntimeConfig`s — but it constructs both arms manually
// and never calls `run_exec_inline_with_forward` itself, so it would not
// notice a revert of the actual `compute_config_id(&cfg, Some(&khive_cfg))`
// call at the real call site above. This test closes that gap: it drives
// `run_exec_inline_with_forward` for real, against a project-local
// `.khive/config.toml` that declares a genuine multi-backend topology, and
// captures the DAEMON REQUEST FRAME's actual `config_id` via a spy — the
// exact value that would be sent over the wire to a real daemon.

#[cfg(unix)]
std::thread_local! {
    static SPY_CAPTURED_CONFIG_ID: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static SPY_CAPTURED_CONFIG_PATH: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    static SPY_CAPTURED_DB: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
    static SPY_CAPTURED_PACKS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(unix)]
fn spy_capture_config_id<'a>(
    frame: &'a DaemonRequestFrame,
    config: Option<PathBuf>,
    db: Option<&'a str>,
    packs: Vec<String>,
) -> super::ForwardFuture<'a> {
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = config);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = db.map(str::to_string));
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = Some(packs));
    Box::pin(async { None })
}

#[cfg(unix)]
fn spy_capture_config_and_succeed<'a>(
    frame: &'a DaemonRequestFrame,
    config: Option<PathBuf>,
    db: Option<&'a str>,
    packs: Vec<String>,
) -> super::ForwardFuture<'a> {
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = config);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = db.map(str::to_string));
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = Some(packs));
    Box::pin(async {
        Some(Ok(
            r#"{"results":[{"ok":true,"tool":"stats","result":{}}],"summary":{"total":1,"succeeded":1,"failed":0}}"#
                .to_string(),
        ))
    })
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn explicit_config_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);

    let dir = tempfile::tempdir().expect("config tempdir");
    let config_path = dir.path().join("selected.toml");
    std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n").expect("write explicit config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        Some(config_path),
        "the daemon spawn seam must receive the same explicit config path used to resolve the exec frame"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn memory_db_override_reaches_daemon_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

    let dir = tempfile::tempdir().expect("config tempdir");
    let config_path = dir.path().join("selected.toml");
    std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n").expect("write explicit config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(":memory:".to_string()),
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;

    assert!(
        result.is_ok(),
        "forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        Some(":memory:".to_string()),
        "the daemon spawn seam must receive the raw --db override so a spawned daemon \
             can be constructed with the same ephemeral in-memory storage"
    );
}

/// A declared read-only SQLite `main` backend becomes a writable memory
/// backend when `--db :memory:` forces the whole topology ephemeral. The
/// pre-open exec frame must fingerprint that effective runtime mode, not
/// the superseded declaration, or the freshly spawned daemon rejects its
/// very first request as a config mismatch.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn force_memory_exec_frame_matches_opened_read_only_topology_runtime() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    let (prev_home, _home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().expect("force-memory config tempdir");
    let config_path = fixture.path().join("read-only-topology.toml");
    let declared_main = fixture.path().join("declared-main.db");
    let declared_archive = fixture.path().join("declared-archive.db");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"
read_only = true

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
"#,
            declared_main.display(),
            declared_archive.display(),
        ),
    )
    .expect("write read-only topology config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: Some(&config_path),
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve force-memory exec config");
    assert_eq!(
        cfg.db_path, None,
        "the force-memory anchor must be in-memory"
    );

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(":memory:".to_string()),
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(result.is_ok(), "force-memory dispatch failed: {result:?}");

    let frame_config_id = SPY_CAPTURED_CONFIG_ID
        .with(|captured| captured.borrow_mut().take())
        .expect("spy must capture the forwarded config id");
    let khive_cfg = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
        .expect("load force-memory topology")
        .expect("explicit config must exist");
    let opened =
        khive_mcp::serve::build_registry_for_multi_backend(cfg, &khive_cfg, Some(":memory:"))
            .await
            .expect("force-memory runtime must build");
    restore_home(prev_home);

    assert!(
        !opened.default_runtime.is_read_only(),
        "force-memory replaces the declared read-only SQLite main with writable memory"
    );
    assert_eq!(
        frame_config_id, opened.config_id,
        "the pre-open exec frame and opened force-memory runtime must have identical config ids"
    );
    assert!(
        !declared_main.exists() && !declared_archive.exists(),
        "force-memory parity setup must not materialize either declared SQLite path"
    );
}
