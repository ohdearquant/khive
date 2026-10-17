// ── #1226: inline --db/[[backends]] guard must fire before daemon-forward ──

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn inline_db_override_guard_normalizes_main_config_id_and_rejects_conflict() {
    if crate::test_process::run_in_child() {
        return;
    }

    std::env::remove_var("KHIVE_EMBEDDING_MODEL");
    std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    let (prev_home, home_dir) = isolate_home_for_test();
    SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

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

    let no_override_cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: None,
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        // Pin the pack list rather than inheriting `KHIVE_PACKS` from the
        // ambient environment (#1276) — an ambient list naming packs not
        // compiled into this build would fail resolution before the
        // behavior under test.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config without override");
    let no_override_result = run_exec_inline_with_forward(
        "stats()".to_string(),
        no_override_cfg,
        None,
        None,
        None,
        ExecDbContext::default(),
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(
        no_override_result.is_ok(),
        "no-override dispatch must succeed: {no_override_result:?}"
    );
    let no_override_config_id = SPY_CAPTURED_CONFIG_ID
        .with(|captured| captured.borrow_mut().take())
        .expect("no-override frame must be captured");

    let matching_override = main_backend_path.display().to_string();
    // Sentinel: proves a captured None below means "the seam was called
    // with None" (withheld override), not "the spy was never invoked".
    SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = Some("sentinel".to_string()));
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: Some(&matching_override),
        config: None,
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        // Pin the pack list rather than inheriting `KHIVE_PACKS` from the
        // ambient environment (#1276) — an ambient list naming packs not
        // compiled into this build would fail resolution before the
        // behavior under test.
        packs: Some(vec!["kg".to_string()]),
        brain_profile: None,
    })
    .expect("resolve exec-shaped config");

    let matching_result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg.clone(),
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(matching_override.clone()),
            anchor: khive_runtime::resolve_db_anchor(Some(&matching_override)),
            config: None,
        },
        false,
        spy_capture_config_id,
    )
    .await;
    assert!(
        matching_result.is_ok(),
        "an override matching the declared main backend must reach daemon forwarding: {matching_result:?}"
    );
    let matching_config_id = SPY_CAPTURED_CONFIG_ID
        .with(|captured| captured.borrow_mut().take())
        .expect("matching-override frame must be captured");
    assert_eq!(
        SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
        None,
        "the redundant multi-backend concrete override must be WITHHELD from the spawn \
             seam: the frame's fingerprint is normalized to the no-override anchor, and the \
             spawned daemon's config-declared main path IS the override's target"
    );

    let conflicting_override = backend_dir.path().join("override.db");
    let result = run_exec_inline_with_forward(
        "stats()".to_string(),
        cfg,
        None,
        None,
        None,
        ExecDbContext {
            raw: Some(conflicting_override.display().to_string()),
            anchor: None,
            config: None,
        },
        false,
        spy_capture_config_id,
    )
    .await;
    restore_home(prev_home);

    assert!(
        result.is_err(),
        "a --db/KHIVE_DB override that conflicts with a declared [[backends]] topology \
             must be rejected on the inline path too, not only on --ops-file; got: {result:?}"
    );
    assert!(
        SPY_CAPTURED_CONFIG_ID.with(|c| c.borrow().is_none()),
        "the conflict must be caught BEFORE any daemon-forward attempt — the spy must \
             never have been called"
    );
    assert_eq!(
        matching_config_id, no_override_config_id,
        "a matching --db override must emit the same config_id as no override for the same multi-backend config"
    );
}
