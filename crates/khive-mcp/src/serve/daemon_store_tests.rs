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

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_is_resolved_canonical_complete() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("configured/main.db");
    let secondary = root.join("unassigned/second.db");
    let home_anchor = root.join("unrelated-home/khive.db");
    let mut config = sqlite_multi_backend_config(main.clone(), secondary.clone());
    config.packs.clear();
    config.backends[1].read_only = true;
    let resolved = RuntimeConfig {
        db_path: Some(home_anchor.clone()),
        events_split: Some(khive_runtime::events_split::EventsSplitConfig {
            db_path: root.join("unrelated-home/khive.db.events.db"),
            socket_path: None,
        }),
        ..RuntimeConfig::default()
    };
    let set = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    assert_eq!(set.main(), main);
    assert_eq!(
        set.paths(),
        &[
            main.clone(),
            root.join("configured/main.db.events.db"),
            secondary
        ]
    );
    assert_eq!(
        resolved.db_path.as_ref(),
        Some(&home_anchor),
        "resolved input must remain unchanged"
    );
    for missing in ["configured", "unassigned", "unrelated-home"] {
        assert!(
            !root.join(missing).exists(),
            "metadata discovery created {missing}"
        );
    }
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_events_modes_match_actual_topology() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let implicit_main = root.join("implicit.db");
    let main = root.join("declared.db");
    let second = root.join("second.db");
    let declared = sqlite_multi_backend_config(main.clone(), second.clone());
    for forwarding in [None, Some(false), Some(true)] {
        let events = root.join("implicit.db.events.db");
        let resolved = RuntimeConfig {
            db_path: Some(implicit_main.clone()),
            events_split: forwarding.map(|socket| khive_runtime::events_split::EventsSplitConfig {
                db_path: events.clone(),
                socket_path: socket.then(|| root.join("not-a-store.sock")),
            }),
            ..RuntimeConfig::default()
        };
        let implicit = maintenance_store_set(&resolved, &KhiveConfig::default(), false)
            .unwrap()
            .unwrap();
        let mut expected = vec![implicit_main.clone()];
        if forwarding.is_some() {
            expected.push(events);
        }
        assert_eq!(
            implicit.paths(),
            expected,
            "implicit captured split must survive"
        );
        let configured = maintenance_store_set(&resolved, &declared, false)
            .unwrap()
            .unwrap();
        let mut expected = vec![main.clone(), second.clone()];
        if forwarding.is_some() {
            expected.insert(1, root.join("declared.db.events.db"));
        }
        assert_eq!(
            configured.paths(),
            expected,
            "direct/socket split uses actual declared main"
        );
        let mut boot = resolved.clone();
        normalize_redundant_db_override(&mut boot, None, &declared.backends).unwrap();
        super::storage_topology::reanchor_declared_events_split(&mut boot, &declared.backends);
        assert_eq!(boot.db_path.as_deref(), Some(configured.main()));
        assert_eq!(
            boot.events_split.as_ref().map(|s| &s.db_path),
            forwarding.map(|_| &configured.paths()[1])
        );
    }
    assert!(!implicit_main.exists() && !main.exists() && !second.exists());
    assert!(!root.join("implicit.db.events.db").exists());
    assert!(!root.join("declared.db.events.db").exists());
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_aliases_and_main_role_are_stable() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let real = root.join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let main = real.join("z-main.db");
    let second = real.join("a-side.db");
    let mut config = sqlite_multi_backend_config(alias.join("z-main.db"), second.clone());
    let mut duplicate = config.backends[0].clone();
    duplicate.name = "same-file".into();
    duplicate.path = Some(main.clone());
    config.backends.push(duplicate);
    let resolved = RuntimeConfig {
        db_path: None,
        events_split: None,
        ..RuntimeConfig::default()
    };
    let first = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    config.backends.reverse();
    let reordered = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    assert_eq!(first, reordered);
    assert_eq!(first.paths(), &[second.clone(), main.clone()]);
    assert_eq!(first.main(), main);
    for backend in &mut config.backends {
        if backend.name == "main" {
            backend.name = "former-main".into();
        } else if backend.name == "secondary" {
            backend.name = "main".into();
        }
    }
    let changed_main = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    assert_eq!(first.paths(), changed_main.paths());
    assert_eq!(changed_main.main(), second);
    assert_ne!(first.digest(), changed_main.digest());
    assert!(!main.exists() && !second.exists());
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_memory_and_missing_paths_are_explicit() {
    let fixture = tempfile::tempdir().unwrap();
    let main = fixture.path().join("main.db");
    let second = fixture.path().join("second.db");
    let mut config = sqlite_multi_backend_config(main.clone(), second.clone());
    let resolved = RuntimeConfig {
        db_path: Some(main.clone()),
        events_split: None,
        ..RuntimeConfig::default()
    };
    assert!(maintenance_store_set(&resolved, &config, true)
        .unwrap()
        .is_none());
    let mut all_memory = config.clone();
    for backend in &mut all_memory.backends {
        backend.kind = BackendKind::Memory;
        backend.path = None;
    }
    assert!(maintenance_store_set(&resolved, &all_memory, false)
        .unwrap()
        .is_none());
    assert!(maintenance_store_set(
        &RuntimeConfig {
            db_path: None,
            ..resolved.clone()
        },
        &KhiveConfig::default(),
        false
    )
    .unwrap()
    .is_none());
    config.backends[1].path = None;
    let missing_secondary = maintenance_store_set(&resolved, &config, false).unwrap_err();
    assert!(missing_secondary.to_string().contains("secondary"));
    config.backends[1].path = Some(second.clone());
    config.backends[0].path = None;
    let missing_path = maintenance_store_set(&resolved, &config, false).unwrap_err();
    assert_eq!(
        missing_path.to_string(),
        "sqlite backend main requires a path field"
    );
    config.backends.remove(0);
    let missing_main = maintenance_store_set(&resolved, &config, false).unwrap_err();
    assert!(missing_main.to_string().contains("declared main"));
    assert!(!main.exists() && !second.exists());
}

#[tokio::test]
#[serial]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
async fn maintenance_mixed_memory_refusal_does_not_change_ordinary_boot() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    initialize_claimed_file_observer_in_child();
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("unused-main.db");
    let second = root.join("secondary.db");
    let mut config = sqlite_multi_backend_config(main.clone(), second.clone());
    config.backends[0].kind = BackendKind::Memory;
    config.backends[0].path = None;
    let resolved = base_runtime_config_for_multi_backend();
    let error = maintenance_store_set(&resolved, &config, false).unwrap_err();
    let refusal = error
        .downcast_ref::<MaintenanceTopologyRefusal>()
        .expect("typed topology refusal");
    assert_eq!(
        refusal,
        &MaintenanceTopologyRefusal::MemoryMainWithPersistentStores {
            paths: vec![second.clone()],
        }
    );
    assert!(
        !main.exists() && !second.exists(),
        "maintenance preparation must not open stores"
    );
    // Maintenance-only refusal must not become an ordinary no-declaration boot policy.
    let multi = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        build_registry_for_multi_backend(resolved, &config, None),
    )
    .await
    .expect("ordinary boot must finish")
    .expect("memory main with a file secondary still boots");
    assert_eq!(multi.default_runtime.backend_id().as_str(), "main");
    assert_eq!(
        multi.per_pack_runtimes["comm"].backend_id().as_str(),
        "secondary"
    );
    assert_eq!(secondary_file_backed_pools(&multi).len(), 1);
    assert!(
        second.exists(),
        "actual ordinary boot must open the configured persistent secondary"
    );
    assert!(
        !main.exists(),
        "memory main must not materialize its discarded path"
    );
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_refuses_existing_hard_link_members() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("z-main.db");
    let secondary = root.join("a-side.db");
    let contents = b"metadata-only fixture";
    std::fs::write(&main, contents).unwrap();
    std::fs::hard_link(&main, &secondary).unwrap();
    let mut config = sqlite_multi_backend_config(main.clone(), secondary.clone());
    let resolved = RuntimeConfig {
        db_path: Some(main.clone()),
        events_split: None,
        ..RuntimeConfig::default()
    };
    let expected = MaintenanceTopologyRefusal::HardLinkedStores {
        first: secondary.clone(),
        second: main.clone(),
    };
    for _ in 0..2 {
        let configured_paths = config
            .backends
            .iter()
            .map(|backend| (backend.name.clone(), backend.path.clone()))
            .collect::<Vec<_>>();
        let error = maintenance_store_set(&resolved, &config, false).unwrap_err();
        assert_eq!(
            error.downcast_ref::<MaintenanceTopologyRefusal>(),
            Some(&expected)
        );
        assert_eq!(
            config
                .backends
                .iter()
                .map(|backend| (backend.name.clone(), backend.path.clone()))
                .collect::<Vec<_>>(),
            configured_paths,
            "metadata preparation must preserve configured spellings"
        );
        assert_eq!(resolved.db_path.as_ref(), Some(&main));
        config.backends.reverse();
    }
    assert!(maintenance_store_set(&resolved, &config, true)
        .unwrap()
        .is_none());
    assert_eq!(std::fs::read(&main).unwrap(), contents);
    assert_eq!(std::fs::read(&secondary).unwrap(), contents);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);

    // Equal contents in separate files are healthy, and main is not first.
    std::fs::remove_file(&secondary).unwrap();
    std::fs::write(&secondary, contents).unwrap();
    let stores = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    let expected = khive_db::maintenance::StoreSet::from_canonical(
        main.clone(),
        vec![main.clone(), secondary.clone()],
    )
    .unwrap();
    assert_eq!(stores, expected);
    assert_eq!(stores.main(), main);
    assert_eq!(stores.paths(), &[secondary, main]);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_refuses_secondary_and_events_hard_links() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("main.db");
    let first = root.join("a-side.db");
    let second = root.join("b-side.db");
    std::fs::write(&main, b"main").unwrap();
    std::fs::write(&first, b"side").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let mut config = sqlite_multi_backend_config(main.clone(), first.clone());
    let mut third = config.backends[1].clone();
    third.name = "third".into();
    third.path = Some(second.clone());
    config.backends.push(third);
    let mut resolved = RuntimeConfig {
        db_path: Some(main.clone()),
        events_split: None,
        ..RuntimeConfig::default()
    };
    let error = maintenance_store_set(&resolved, &config, false).unwrap_err();
    assert_eq!(
        error.downcast_ref::<MaintenanceTopologyRefusal>(),
        Some(&MaintenanceTopologyRefusal::HardLinkedStores {
            first: first.clone(),
            second: second.clone(),
        })
    );

    let events = root.join("main.db.events.db");
    std::fs::hard_link(&main, &events).unwrap();
    resolved.events_split = Some(khive_runtime::events_split::EventsSplitConfig {
        db_path: events.clone(),
        socket_path: None,
    });
    let declared = sqlite_multi_backend_config(main.clone(), first.clone());
    let implicit = KhiveConfig::default();
    for config in [&implicit, &declared] {
        let error = maintenance_store_set(&resolved, config, false).unwrap_err();
        assert_eq!(
            error.downcast_ref::<MaintenanceTopologyRefusal>(),
            Some(&MaintenanceTopologyRefusal::HardLinkedStores {
                first: main.clone(),
                second: events.clone(),
            })
        );
    }
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 4);

    std::fs::remove_file(&events).unwrap();
    std::fs::write(&events, b"main").unwrap();
    let stores = maintenance_store_set(&resolved, &declared, false)
        .unwrap()
        .unwrap();
    assert_eq!(stores.main(), main);
    assert_eq!(stores.paths(), &[first, main, events]);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 4);
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn maintenance_store_set_accepts_existing_canonical_aliases_and_absent_members() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("main.db");
    let alias = root.join("alias.db");
    std::fs::write(&main, b"main").unwrap();
    std::os::unix::fs::symlink(&main, &alias).unwrap();
    let mut config = sqlite_multi_backend_config(main.clone(), alias.clone());
    let resolved = RuntimeConfig {
        db_path: Some(main.clone()),
        events_split: None,
        ..RuntimeConfig::default()
    };
    let stores = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    assert_eq!(stores.paths(), std::slice::from_ref(&main));
    assert_eq!(stores.main(), main);
    assert_eq!(config.backends[1].path.as_ref(), Some(&alias));

    let missing = root.join("missing/nested/side.db");
    config.backends[1].path = Some(missing.clone());
    let stores = maintenance_store_set(&resolved, &config, false)
        .unwrap()
        .unwrap();
    assert_eq!(stores.paths(), &[main.clone(), missing.clone()]);
    assert_eq!(stores.main(), main);
    assert_eq!(config.backends[1].path.as_ref(), Some(&missing));
    assert!(!root.join("missing").exists());
    assert_eq!(std::fs::read(&main).unwrap(), b"main");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
}

#[test]
#[cfg(unix)]
fn maintenance_store_identity_probe_preserves_absence_and_io_error_kind() {
    use std::io::{Error, ErrorKind};

    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("missing/z-main.db");
    let secondary = root.join("missing/a-side.db");
    let stores = khive_db::maintenance::StoreSet::from_canonical(
        main,
        vec![secondary.clone(), secondary, root.join("missing/z-main.db")],
    )
    .unwrap();
    let before = stores.clone();
    let mut observed = Vec::new();
    super::server_build::validate_maintenance_store_identities_with(&stores, |path| {
        observed.push(path.to_path_buf());
        Err(Error::new(ErrorKind::NotFound, "controlled absence"))
    })
    .unwrap();
    assert_eq!(observed, stores.paths());
    assert_eq!(stores, before);

    // Each non-NotFound failure must survive even after an absent first member.
    for kind in [ErrorKind::PermissionDenied, ErrorKind::InvalidData] {
        let mut observed = Vec::new();
        let error =
            super::server_build::validate_maintenance_store_identities_with(&stores, |path| {
                observed.push(path.to_path_buf());
                if path == stores.paths()[0].as_path() {
                    Err(Error::new(ErrorKind::NotFound, "controlled absence"))
                } else {
                    Err(Error::new(kind, "controlled metadata failure"))
                }
            })
            .unwrap_err();
        assert_eq!(observed, stores.paths());
        assert_eq!(
            error.to_string(),
            format!(
                "cannot inspect maintenance database identity at {}",
                stores.paths()[1].display()
            )
        );
        let cause = error.downcast_ref::<Error>().expect("original I/O error");
        assert_eq!(cause.kind(), kind);
        assert_eq!(cause.to_string(), "controlled metadata failure");
        assert_eq!(stores, before);
    }
    assert!(!root.join("missing").exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}
