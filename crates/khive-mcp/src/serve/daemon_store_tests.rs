#[test]
#[cfg(unix)]
fn daemon_store_paths_follow_physical_backends_not_the_home_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let main = real.join("main.db");
    let canonical_main = std::fs::canonicalize(&real).unwrap().join("main.db");
    let khive_cfg = sqlite_multi_backend_config(main.clone(), alias.join("main.db"));
    let unrelated_home_anchor = dir.path().join("other-home/.khive/khive.db");

    assert_eq!(
        daemon_store_paths(Some(&unrelated_home_anchor), &khive_cfg.backends, false).unwrap(),
        vec![canonical_main.clone()],
        "two declared aliases must take one store lock independent of HOME"
    );
    let mut boot_db = Some(unrelated_home_anchor.clone());
    let mut boot_anchor = boot_db.clone();
    let mut boot_backends = khive_cfg.backends.clone();
    let plan = prepare_daemon_store_plan(&mut boot_db, &mut boot_anchor, &mut boot_backends, false)
        .unwrap();
    assert_eq!(plan.paths, vec![canonical_main.clone()]);
    assert!(
        boot_backends
            .iter()
            .all(|backend| backend.path.as_deref() == Some(canonical_main.as_path())),
        "every declared alias must open the frozen claimed pathname"
    );
    assert!(
        !main.exists() && !unrelated_home_anchor.exists(),
        "target discovery must not open a database"
    );
    assert!(
        daemon_store_paths(Some(&unrelated_home_anchor), &khive_cfg.backends, true)
            .unwrap()
            .is_empty(),
        ":memory: overrides every declared file backend"
    );
}

#[tokio::test]
#[serial]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
async fn daemon_run_refuses_cross_home_store_overlap_before_opening_sqlite() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    use clap::Parser;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let database = root.join("shared/khive.db");
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = 'main'\npath = {:?}\n",
            database.to_str().unwrap()
        ),
    )
    .unwrap();
    let first = khive_runtime::daemon::acquire_daemon_store_guards([database.clone()])
        .expect("incumbent store claim");
    let second_home = root.join("second-home");
    std::fs::create_dir(&second_home).unwrap();
    let _home_guard = HomeGuard::redirect_to(&second_home);
    let args = Args::parse_from([
        "mcp",
        "--daemon",
        "--config",
        config.to_str().unwrap(),
        "--no-embed",
        "--pack",
        "kg",
    ]);
    let registry = TransportRegistry::with_builtins();
    let error = tokio::time::timeout(std::time::Duration::from_secs(10), run(args, &registry))
        .await
        .expect("store overlap must refuse promptly, before serving")
        .expect_err("daemon cannot serve a store claimed under another HOME");
    let message = error.to_string();
    assert!(message.contains("already running"), "{message}");
    assert!(
        message.contains(&format!("pid {}", std::process::id())),
        "{message}"
    );
    assert!(
        !database.exists(),
        "refusal must precede SQLite construction"
    );
    assert!(!second_home.join(".khive/khive.db").exists());
    drop(first);
}

#[test]
#[cfg(unix)]
fn daemon_store_plan_refuses_retargeted_symlink_before_open() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let first = fixture.path().join("first.db");
    let second = fixture.path().join("second.db");
    std::fs::write(&first, b"first").unwrap();
    std::fs::write(&second, b"second").unwrap();
    let alias = fixture.path().join("configured.db");
    symlink(&first, &alias).unwrap();

    let mut configured = Some(alias.clone());
    let mut anchor = configured.clone();
    let mut backends = Vec::new();
    let plan = prepare_daemon_store_plan(&mut configured, &mut anchor, &mut backends, false)
        .expect("resolve original alias once");
    let claimed = first.canonicalize().unwrap();
    assert_eq!(configured.as_deref(), Some(claimed.as_path()));
    assert_eq!(plan.paths, vec![claimed.clone()]);
    let _guards = khive_runtime::daemon::acquire_daemon_store_guards(plan.paths.clone())
        .expect("claim original target");

    let replacement_alias = fixture.path().join("new-link");
    symlink(&second, &replacement_alias).unwrap();
    std::fs::rename(replacement_alias, &alias).unwrap();
    let error = plan
        .assert_aliases_unchanged()
        .expect_err("retargeting the configured spelling must refuse before open");
    let message = error.to_string();
    assert!(
        message.contains(&claimed.display().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&second.canonicalize().unwrap().display().to_string()),
        "{message}"
    );
}

#[tokio::test]
#[serial]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
async fn daemon_retains_store_guard_while_its_socket_is_serving() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    initialize_claimed_file_observer_in_child();
    use clap::Parser;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let database = root.join("served.db");
    let args = Args::parse_from([
        "mcp",
        "--daemon",
        "--db",
        database.to_str().unwrap(),
        "--no-embed",
        "--pack",
        "kg",
    ]);
    let daemon = tokio::spawn(async move {
        let registry = TransportRegistry::with_builtins();
        run(args, &registry).await
    });
    let socket = khive_runtime::daemon::socket_path();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !socket.exists() {
        assert!(
            !daemon.is_finished(),
            "daemon exited before binding its socket"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not bind its socket"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let error = khive_runtime::daemon::acquire_daemon_store_guards([database])
        .expect_err("a serving daemon must retain its store claim");
    assert!(error.to_string().contains("already running"), "{error}");
    daemon.abort();
    let _ = daemon.await;
}

#[tokio::test]
#[serial]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
async fn daemon_run_serves_chmod_read_only_single_backend_snapshot() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    initialize_claimed_file_observer_in_child();
    use clap::Parser;
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().expect("canonical fixture");
    let database = root.join("snapshot.db");
    let db_arg = database.to_str().expect("utf8 fixture path");
    let seed_args = Args::parse_from(["mcp", "--db", db_arg, "--no-embed", "--pack", "kg"]);
    {
        let (_server, _schedule_rt) = build_server(&seed_args)
            .await
            .expect("seed current kg schema before freezing snapshot");
    }
    let mut permissions = std::fs::metadata(&database).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&database, permissions).unwrap();
    freeze_snapshot_sidecars(&database);

    let mut planned_db = Some(database.clone());
    let mut db_anchor = planned_db.clone();
    let mut backends = Vec::new();
    let plan = prepare_daemon_store_plan(&mut planned_db, &mut db_anchor, &mut backends, false)
        .expect("plan the undeclared single-backend store");
    assert_eq!(plan.paths, vec![database.clone()]);
    assert_eq!(
        plan.read_only_paths,
        vec![database.clone()],
        "the daemon claim must use the backend's chmod-detected read-only mode"
    );

    let args = Args::parse_from([
        "mcp",
        "--daemon",
        "--db",
        db_arg,
        "--no-embed",
        "--pack",
        "kg",
    ]);
    let daemon = tokio::spawn(async move {
        let registry = TransportRegistry::with_builtins();
        run(args, &registry).await
    });
    let socket = khive_runtime::daemon::socket_path();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    while !socket.exists() {
        if daemon.is_finished() {
            let outcome = daemon.await.expect("join daemon boot");
            panic!("read-only daemon exited before binding its socket: {outcome:?}");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "read-only daemon did not bind its socket"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    daemon.abort();
    let _ = daemon.await;
}

/// The daemon host installs the claimed-file observer at process startup. A
/// test that drives `run` in-process must do the same in its isolated child.
#[cfg(unix)]
fn initialize_claimed_file_observer_in_child() {
    // SAFETY: the isolation helper selected only this test in a fresh child;
    // the fixture has not opened any SQLite database yet.
    unsafe { khive_db::pool::initialize_claimed_file_observer().unwrap() };
}
