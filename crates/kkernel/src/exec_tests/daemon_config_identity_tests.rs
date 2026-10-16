/// The redundant-multi-backend spawn decision (override withheld from the
/// spawned daemon) has a config-side twin: when no explicit `--config`
/// was given, the config that declared the backend topology was
/// DISCOVERED (here via the db-dir tier-3 anchor of
/// `KhiveConfig::load_with_home_fallback_and_source`), and the withheld
/// override was the child's only other clue about which database to
/// bind. The spawn seam must receive that retained resolved path as the
/// child's explicit `--config`, or the spawned daemon re-discovers from
/// its own cwd/HOME, cannot reach a config anchored only beside the
/// database, binds `$HOME/.khive/khive.db`, and squats the socket with a
/// `config_id` that never matches the normalized frame.
///
/// Control arms: with an explicit config the seam receives the explicit
/// path (never the discovered one), and in the empty-backends case the
/// seam receives no config at all (the concrete override supplies the
/// database directly).
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn redundant_db_override_forwards_discovered_config_to_spawn_seam() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    std::env::remove_var("KHIVE_DB");
    let (prev_home, _home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

    // A multi-backend config discoverable ONLY via the db-dir tier-3
    // anchor (`project_config_anchor_dir`): it lives in
    // `<main-db-dir>/.khive/config.toml`, not at the process cwd and not
    // under `$HOME/.khive`.
    let backend_dir = tempfile::tempdir().expect("backend tempdir");
    let main_backend_path = backend_dir.path().join("main-backend.db");
    let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
    let anchor_dir = backend_dir.path().join(".khive");
    std::fs::create_dir_all(&anchor_dir).expect("mkdir db-dir anchor");
    let discovered_config_path = anchor_dir.join("config.toml");
    std::fs::write(
        &discovered_config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write tier-3 multi-backend config");
    let canonical_config_path =
        std::fs::canonicalize(&discovered_config_path).expect("canonicalize config path");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(main_backend_path.to_str().expect("utf8")),
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    // ── the fix case: redundant multi-backend, no explicit config ──
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(main_backend_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(main_backend_path.to_str()),
            config: None,
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(
        result.is_ok(),
        "redundant-override dispatch must reach daemon forwarding: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        None,
        "the redundant override stays withheld from the spawn seam"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        Some(canonical_config_path.clone()),
        "the spawn seam must receive the retained resolved config path as the \
             child's explicit --config when the redundant override is withheld"
    );

    // ── control: explicit config wins, discovered path is not substituted ──
    let explicit_config_path = backend_dir.path().join("explicit.toml");
    std::fs::copy(&discovered_config_path, &explicit_config_path)
        .expect("copy topology as explicit config");
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(main_backend_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(main_backend_path.to_str()),
            config: Some(explicit_config_path.clone()),
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(
        result.is_ok(),
        "explicit-config dispatch must reach daemon forwarding: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        Some(explicit_config_path),
        "with an explicit config the seam receives the operator's path, never a discovered one"
    );

    // ── control: empty backends get no config, only the concrete override ──
    let single_dir = tempfile::tempdir().expect("single-backend tempdir");
    let override_path = single_dir.path().join("override.db");
    let single_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(override_path.to_str().expect("utf8")),
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve single-backend exec-shaped config");
    SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        single_cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(override_path.display().to_string()),
            anchor: khive_runtime::resolve_db_anchor(override_path.to_str()),
            config: None,
        },
        false,
        spy_capture_config_and_succeed,
    )
    .await;
    assert!(
        result.is_ok(),
        "single-backend dispatch must reach daemon forwarding: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
        None,
        "the empty-backends case forwards no config — the concrete override supplies the database"
    );
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        Some(override_path.display().to_string()),
        "the single-backend concrete override still reaches the spawn seam"
    );

    restore_home(prev_home);
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn explicit_config_is_loaded_for_exec_forward_frame() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    let (prev_home, _home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let fixture = tempfile::tempdir().expect("config fixture tempdir");
    let config_path = fixture.path().join("code-map.toml");
    let main_backend_path = fixture.path().join("code-map.db");
    let sessions_backend_path = fixture.path().join("sessions.db");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write explicit exec config");

    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: Some(&config_path),
        namespace: Namespace::parse("local").expect("namespace"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve explicit exec config");

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: None,
            anchor: None,
            config: Some(config_path.clone()),
        },
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(
        result.is_ok(),
        "explicit-config dispatch failed: {result:?}"
    );

    let captured = SPY_CAPTURED_CONFIG_ID
        .with(|value| value.borrow_mut().take())
        .expect("spy must capture the forwarded config id");
    let khive_cfg = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
        .expect("load explicit config")
        .expect("explicit config must exist");
    let mut expected_config = cfg;
    expected_config.db_path = Some(main_backend_path);
    let expected = compute_config_id(&expected_config, Some(&khive_cfg));
    restore_home(prev_home);

    assert_eq!(
        captured, expected,
        "the exec forward frame must fold the explicitly selected backend topology"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn exec_frame_config_id_matches_daemon_config_id_for_multi_backend_project_toml() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    let (prev_home, home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

    // No explicit `--db` anywhere below — this mirrors the real multi-tenant
    // deployment shape the bug affects: `~/.khive/config.toml` declares
    // `[[backends]]` and `kkernel exec` relies on default discovery.
    // A divergent explicit `--db` would be rejected as ambiguous once
    // backends are declared; repeating the main path would be accepted but
    // would not model this default-discovery scenario.
    let khive_dir = home_dir.path().join(".khive");
    std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
    // Keep the configuration home-shaped while placing the stores in a
    // separate tempdir. Test-harness builds reject every store under
    // `$HOME/.khive`, including isolated fixtures, at the open boundary.
    let backend_dir = tempfile::tempdir().expect("backend tempdir");
    let main_backend_path = backend_dir.path().join("main-backend.db");
    let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
    std::fs::write(
        khive_dir.join("config.toml"),
        format!(
            r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "sessions"
kind = "sqlite"
path = "{}"

[packs.session]
backend = "sessions"
"#,
            main_backend_path.display(),
            sessions_backend_path.display(),
        ),
    )
    .expect("write multi-backend config.toml");

    // `no_embed: true` keeps this test fast and network-independent — it is
    // scoped to the backends-topology fold, not embedding-model resolution
    // (a separate, already-covered concern in the sibling project-toml test).
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (#1276) — this test's assertion is
        // about config_id parity, not about pack resolution.
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
        ExecDbContext::default(),
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(result.is_ok(), "exec dispatch must succeed: {result:?}");

    let captured = SPY_CAPTURED_CONFIG_ID
        .with(|c| c.borrow_mut().take())
        .expect("spy must have captured a forwarded frame");

    // Independently compute what the DAEMON would compute for the exact
    // same on-disk config.toml + database, mirroring serve.rs's own boot
    // path (`build_server`): resolve_runtime_config with
    // namespace_explicit=false (the daemon-startup shape), load the same
    // KhiveConfig, and fold it with Some(&khive_cfg) exactly like
    // serve.rs:916 does.
    let serve_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: true,
        // Same pin as `cfg` above (#1276) — both sides of the parity
        // comparison must resolve identically regardless of ambient
        // `KHIVE_PACKS`.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve serve-shaped config");
    let khive_cfg = KhiveConfig::load_with_home_fallback(None, serve_cfg.db_path.as_deref())
        .expect("load multi-backend config.toml")
        .expect("config.toml must be found at tier 3");
    assert!(
        !khive_cfg.backends.is_empty(),
        "sanity: the written config.toml must actually resolve with a non-empty \
             backends list, or this test proves nothing"
    );
    let mut declared_main_config = serve_cfg;
    declared_main_config.db_path = Some(main_backend_path);
    let daemon_config_id = compute_config_id(&declared_main_config, Some(&khive_cfg));
    restore_home(prev_home);

    assert_eq!(
        captured, daemon_config_id,
        "the config_id in the ACTUAL frame run_exec_inline_with_forward sends to the \
             daemon must be byte-identical to what the daemon computes for the same \
             multi-backend config.toml (D1 acceptance gate, exercised end-to-end through \
             the real call site rather than a standalone compute_config_id comparison)"
    );
}

/// With no --db override, a declared memory main replaces the unused
/// HOME-shaped anchor before forwarding. If no daemon answers, local
/// construction must receive that normalized anchor too.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn declared_topology_exec_fallback_updates_omitted_db_anchor() {
    if crate::test_process::run_in_child() {
        return;
    }

    let fixture = tempfile::tempdir().expect("config fixture");
    let unused_home = fixture.path().join("unused-home");
    let anchor = unused_home.join(".khive/khive.db");
    let config_path = fixture.path().join("memory-topology.toml");
    std::fs::write(
        &config_path,
        "[[backends]]\nname = \"main\"\nkind = \"memory\"\n",
    )
    .expect("write explicit memory topology");
    let cfg = RuntimeConfig {
        db_path: Some(anchor.clone()),
        actor_id: Some("config-anchor-fallback-test".to_string()),
        packs: vec!["kg".to_string()],
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    };
    let topology = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
        .unwrap()
        .expect("explicit topology exists");
    let mut expected_cfg = cfg.clone();
    expected_cfg.db_path = None;
    let expected_id = compute_config_id(&expected_cfg, Some(&topology));
    SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        Some("json".to_string()),
        None,
        ExecDbContext {
            raw: None,
            anchor: Some(anchor),
            config: Some(config_path),
        },
        true,
        spy_capture_config_id,
    )
    .await;

    assert!(result.is_ok(), "local fallback must dispatch: {result:?}");
    assert_eq!(
        SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow_mut().take()),
        Some(expected_id),
        "the forward attempt must use declared memory before falling back"
    );
    assert!(
        !unused_home.exists(),
        "the superseded HOME-shaped database anchor must never be opened"
    );
}
