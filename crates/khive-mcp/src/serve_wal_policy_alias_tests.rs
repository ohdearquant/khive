fn duplicate_sqlite_path_config(db_path: &std::path::Path) -> KhiveConfig {
    use khive_runtime::PackConfig;

    KhiveConfig {
        backends: vec![
            BackendConfig {
                name: "main".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(db_path.to_path_buf()),
                cache_mb: None,
                journal_mode: None,
                wal_ceiling_bytes: None,
                disk_reserve_bytes: None,
                disk_guard_deadline_ms: None,
                served_kinds: None,
                read_only: false,
            },
            BackendConfig {
                name: "alias".to_string(),
                kind: BackendKind::Sqlite,
                path: Some(db_path.to_path_buf()),
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
            let mut packs = std::collections::HashMap::new();
            packs.insert(
                "comm".to_string(),
                PackConfig {
                    backend: "alias".to_string(),
                    verbs_disabled: Vec::new(),
                    no_embed: false,
                },
            );
            packs
        },
        ..KhiveConfig::default()
    }
}

#[test]
fn wal_disclosure_memory_main_does_not_disable_file_secondary() {
    let config = RuntimeConfig {
        db_path: None,
        wal_ceiling_configured_bytes: 8192,
        wal_ceiling_bytes: 8192,
        wal_ceiling_source: khive_runtime::WalCeilingSource::Environment,
        wal_ceiling_env_raw: Some("8192".into()),
        ..RuntimeConfig::no_embeddings()
    };
    let mut topology = duplicate_sqlite_path_config(std::path::Path::new("unused-secondary.db"));
    topology.backends[0].kind = BackendKind::Memory;
    topology.backends[0].path = None;
    let line = resolved_wal_ceiling_disclosure(&config, &topology.backends, false);
    assert!(line.contains("alias: configured_bytes=8192 effective_bytes=8192 source=environment enabled=true status=enforced"), "SECONDARY_FILE_POLICY_DISCLOSURE: {line}");
    assert!(line.contains(
        "main: configured_bytes=0 effective_bytes=0 source=default enabled=false status=disabled"
    ));
    let forced = resolved_wal_ceiling_disclosure(&config, &topology.backends, true);
    assert_eq!(
        forced
            .matches("effective_bytes=0 source=default enabled=false")
            .count(),
        2
    );
}

#[test]
fn wal_ceiling_aliases_reject_unequal_effective_limits_before_open() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("not-created.db");
    let mut topology = duplicate_sqlite_path_config(&path);
    topology.backends[0].wal_ceiling_bytes = Some(8192);
    topology.backends[1].wal_ceiling_bytes = Some(16384);
    let runtime = RuntimeConfig::default();

    let error = validate_wal_ceiling_topology(&runtime, &topology.backends, false)
        .expect_err("one physical writer cannot have two limits");
    assert!(matches!(
        error.downcast_ref::<khive_runtime::ConfigError>(),
        Some(khive_runtime::ConfigError::WalCeilingAliasConflict { .. })
    ));
    assert!(!path.exists(), "static validation must not open a database");

    topology.backends[0].read_only = true;
    topology.backends[1].read_only = true;
    validate_wal_ceiling_topology(&runtime, &topology.backends, false)
        .expect("read-only aliases both enforce zero");
}

#[cfg(unix)]
#[test]
fn wal_ceiling_missing_paths_share_identity_through_symlink_parent() {
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("wal-alias-")
        .tempdir()
        .unwrap();
    // macOS temporary roots can be symlink aliases; keep the physical parent explicit.
    let root = dir.path().canonicalize().unwrap();
    let real = root.join("real");
    let link = root.join("link");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let database = real.join("not-created/deeper/database.db");
    // Reach the Unix root from the physical cwd without changing process-global cwd.
    let mut relative = std::path::PathBuf::new();
    for _ in cwd.components().skip(1) {
        relative.push("..");
    }
    relative.push(link.strip_prefix("/").unwrap());
    let relative = relative.join("not-created/deeper/database.db");
    assert!(
        relative.is_relative(),
        "the second trial must remain relative"
    );
    for alias in [link.join("not-created/deeper/database.db"), relative] {
        let mut topology = duplicate_sqlite_path_config(&database);
        topology.backends[1].path = Some(alias);
        topology.backends[0].wal_ceiling_bytes = Some(8192);
        topology.backends[1].wal_ceiling_bytes = Some(16384);
        let error = validate_wal_ceiling_topology(
            &RuntimeConfig::no_embeddings(),
            &topology.backends,
            false,
        )
        .expect_err("MISSING_ALIAS_POLICY_CONFLICT");
        assert!(matches!(
            error.downcast_ref::<khive_runtime::ConfigError>(),
            Some(khive_runtime::ConfigError::WalCeilingAliasConflict {
                first_bytes: 8192,
                second_bytes: 16384,
                ..
            })
        ));
        assert!(
            !database.parent().unwrap().exists(),
            "validation must not create missing path components"
        );
    }
}

#[cfg(unix)]
#[test]
fn wal_ceiling_hard_link_aliases_reject_conflicting_policy_before_open() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.db");
    let alias = dir.path().join("alias.db");
    std::fs::write(&main, b"").unwrap();
    std::fs::hard_link(&main, &alias).unwrap();
    let mut topology = duplicate_sqlite_path_config(&main);
    topology.backends[1].path = Some(alias);
    topology.backends[0].wal_ceiling_bytes = Some(8192);
    topology.backends[1].wal_ceiling_bytes = Some(16384);

    let error = validate_wal_ceiling_topology(&RuntimeConfig::default(), &topology.backends, false)
        .expect_err("hard-linked aliases must enforce the same writer ceiling");
    assert!(matches!(
        error.downcast_ref::<khive_runtime::ConfigError>(),
        Some(khive_runtime::ConfigError::WalCeilingAliasConflict {
            first_bytes: 8192,
            second_bytes: 16384,
            ..
        })
    ));
    assert_eq!(std::fs::metadata(&main).unwrap().len(), 0);

    for backend in &mut topology.backends {
        backend.read_only = true;
    }
    validate_wal_ceiling_topology(&RuntimeConfig::default(), &topology.backends, false)
        .expect("read-only hard-linked aliases enforce zero despite configured values");
}

#[cfg(unix)]
#[test]
fn wal_ceiling_cached_hard_link_alias_rejects_conflicting_policy() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.db");
    let alias = dir.path().join("alias.db");
    drop(rusqlite::Connection::open(&main).unwrap());
    std::fs::hard_link(&main, &alias).unwrap();
    let mut topology = duplicate_sqlite_path_config(&main);
    topology.backends[1].path = Some(alias);
    topology.backends[0].wal_ceiling_bytes = Some(0);
    topology.backends[1].wal_ceiling_bytes = Some(8192);
    let mut opened = Vec::new();

    let error = open_effective_backends_with(
        &RuntimeConfig::default(),
        &topology.backends,
        None,
        |cfg, max_readers, policy| {
            opened.push(cfg.name.clone());
            assert_eq!(policy.bytes, 0, "only the disabled main should be opened");
            assert_eq!(policy.source, khive_db::WalCeilingSource::BackendField);
            open_backend_with_wal_ceiling(cfg, max_readers, policy)
        },
    )
    .err()
    .expect("cached physical aliases must not silently inherit another writer policy");
    assert_eq!(opened, ["main"]);
    assert!(matches!(
        error.downcast_ref::<khive_runtime::ConfigError>(),
        Some(khive_runtime::ConfigError::WalCeilingAliasConflict {
            first_backend,
            second_backend,
            first_bytes: 0,
            second_bytes: 8192,
            ..
        }) if first_backend == "main" && second_backend == "alias"
    ));
}

#[cfg(unix)]
#[test]
fn wal_ceiling_opener_forwards_each_backend_policy() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.db");
    let alias = dir.path().join("alias.db");
    let secondary = dir.path().join("secondary.db");
    for path in [&main, &secondary] {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE seed (id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    std::fs::hard_link(&main, &alias).unwrap();
    let mut topology = duplicate_sqlite_path_config(&main);
    topology.backends[0].wal_ceiling_bytes = Some(8192);
    topology.backends[1].path = Some(alias);
    topology.backends[1].wal_ceiling_bytes = Some(16384);
    topology.backends.push(BackendConfig {
        name: "secondary".to_string(),
        path: Some(secondary),
        wal_ceiling_bytes: None,
        ..topology.backends[0].clone()
    });
    for backend in &mut topology.backends {
        backend.read_only = true;
    }
    let runtime = RuntimeConfig {
        wal_ceiling_env_raw: Some("32768".to_string()),
        ..RuntimeConfig::default()
    };
    let mut opened = Vec::new();
    let backends = open_effective_backends_with(
        &runtime,
        &topology.backends,
        None,
        |cfg, max_readers, policy| {
            opened.push((cfg.name.clone(), policy));
            open_backend_with_wal_ceiling(cfg, max_readers, policy)
        },
    )
    .unwrap();

    assert_eq!(
        opened.len(),
        2,
        "the physical alias must reuse the main pool"
    );
    assert_eq!(opened[0].0, "main");
    assert_eq!(opened[0].1.bytes, 8192);
    assert_eq!(opened[0].1.source, khive_db::WalCeilingSource::BackendField);
    assert_eq!(opened[1].0, "secondary");
    assert_eq!(opened[1].1.bytes, 32768);
    assert_eq!(opened[1].1.source, khive_db::WalCeilingSource::Environment);
    assert!(Arc::ptr_eq(&backends["main"], &backends["alias"]));
    assert!(!Arc::ptr_eq(&backends["main"], &backends["secondary"]));
    for (name, policy) in opened {
        let pool = backends[&name].pool_arc();
        assert_eq!(pool.config().wal_ceiling, policy);
        assert_eq!(policy.effective_bytes(pool.config().read_only), 0);
    }
}
