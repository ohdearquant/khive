use super::*;
use khive_db::{DiskGuardConfigSource, DiskGuardEnvironment, EffectiveDiskGuardConfig};

fn backend(name: &str, path: PathBuf) -> BackendConfig {
    BackendConfig {
        name: name.into(),
        kind: BackendKind::Sqlite,
        path: Some(path),
        cache_mb: None,
        journal_mode: None,
        served_kinds: None,
        read_only: false,
        wal_ceiling_bytes: None,
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
    }
}

fn runtime(path: PathBuf, locks: PathBuf) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(path),
        volume_lock_dir: Some(locks),
        disk_guard_environment: DiskGuardEnvironment::default(),
        ..RuntimeConfig::no_embeddings()
    }
}

fn environment(reserve: &str, deadline: &str) -> DiskGuardEnvironment {
    DiskGuardEnvironment {
        reserve: Some(reserve.into()),
        deadline: Some(deadline.into()),
        legacy_reserve: None,
    }
}

#[test]
fn captured_disk_policy_survives_environment_change_in_fingerprint_and_open() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    std::env::remove_var("KHIVE_DB_FREE_SPACE_FLOOR_BYTES");
    std::env::set_var("KHIVE_SQLITE_DISK_RESERVE_BYTES", "123");
    std::env::set_var("KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS", "250");
    let dir = tempfile::tempdir().unwrap();
    let mut config = RuntimeConfig {
        db_path: Some(dir.path().join("main.db")),
        volume_lock_dir: Some(dir.path().join("locks")),
        ..RuntimeConfig::no_embeddings()
    };
    resolve_runtime_wal_ceiling(&mut config, &[], false).unwrap();
    let before = crate::server::compute_config_id(&config, None);
    std::env::set_var("KHIVE_SQLITE_DISK_RESERVE_BYTES", "malformed-after-capture");
    std::env::set_var("KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS", "0");
    assert_eq!(before, crate::server::compute_config_id(&config, None));
    let opened = open_single_backend(&mut config, Some(2)).unwrap();
    let policy = opened.pool().effective_disk_guard_config().unwrap();
    assert_eq!((policy.reserve_bytes, policy.guard_deadline_ms), (123, 250));
    assert_eq!(policy.reserve_source, DiskGuardConfigSource::Environment);
    assert_eq!(before, crate::server::compute_config_id(&config, None));
}

#[test]
fn named_disk_policy_identity_uses_effective_numbers_and_stable_order() {
    let dir = tempfile::tempdir().unwrap();
    let config = runtime(dir.path().join("main.db"), dir.path().join("locks"));
    let mut main = backend("main", dir.path().join("main.db"));
    let mut archive = backend("archive", dir.path().join("archive.db"));
    main.disk_reserve_bytes = Some(123);
    archive.disk_reserve_bytes = Some(123);
    main.disk_guard_deadline_ms = Some(250);
    archive.disk_guard_deadline_ms = Some(250);
    let mut topology = KhiveConfig {
        backends: vec![main, archive],
        ..KhiveConfig::default()
    };
    let id = crate::server::compute_config_id(&config, Some(&topology));
    topology.backends.reverse();
    assert_eq!(
        id,
        crate::server::compute_config_id(&config, Some(&topology))
    );
    let mut environment_config = config.clone();
    environment_config.disk_guard_environment = environment("123", "250");
    for b in &mut topology.backends {
        b.disk_reserve_bytes = None;
        b.disk_guard_deadline_ms = None;
    }
    assert_eq!(
        id,
        crate::server::compute_config_id(&environment_config, Some(&topology))
    );
    topology.backends[0].disk_guard_deadline_ms = Some(251);
    assert_ne!(
        id,
        crate::server::compute_config_id(&environment_config, Some(&topology))
    );
    topology.backends[0].disk_guard_deadline_ms = None;
    topology.backends[0].disk_reserve_bytes = Some(124);
    assert_ne!(
        id,
        crate::server::compute_config_id(&environment_config, Some(&topology))
    );
}

#[test]
fn disk_alias_conflict_rejects_before_any_open_and_equal_sources_share_pool() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unopened").join("main.db");
    let mut config = runtime(path.clone(), dir.path().join("locks"));
    config.disk_guard_environment.reserve = Some("123".into());
    let first = backend("main", path.clone());
    let mut second = backend("alias", path.clone());
    second.disk_guard_deadline_ms = Some(250);
    let mut topology = vec![first, second];
    let error = validate_wal_ceiling_topology(&config, &topology, false).unwrap_err();
    assert!(matches!(
        error.downcast_ref::<khive_runtime::ConfigError>(),
        Some(khive_runtime::ConfigError::DiskGuardAliasConflict { .. })
    ));
    assert!(!path.parent().unwrap().exists());
    topology[1].disk_guard_deadline_ms = None;
    topology[1].disk_reserve_bytes = Some(123);
    validate_wal_ceiling_topology(&config, &topology, false).unwrap();
    let opened = open_effective_backends_with(&config, &topology, Some(2), |cfg, readers, wal| {
        open_backend_with_policies(
            cfg,
            readers,
            wal,
            cfg.resolve_disk_guard(&config.disk_guard_environment)?,
            config.volume_lock_dir.as_deref(),
        )
    })
    .unwrap();
    assert!(Arc::ptr_eq(&opened["main"], &opened["alias"]));
    assert_eq!(
        opened["main"]
            .pool()
            .effective_disk_guard_config()
            .unwrap()
            .reserve_bytes,
        123
    );
}

#[cfg(unix)]
#[test]
fn cached_hard_link_alias_rechecks_disk_policy() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("main.db");
    let alias_path = dir.path().join("alias.db");
    let config = runtime(first_path.clone(), dir.path().join("locks"));
    drop(StorageBackend::sqlite_for_test(&first_path).unwrap());
    std::fs::hard_link(&first_path, &alias_path).unwrap();
    let first = backend("main", first_path.clone());
    let mut alias = backend("alias", alias_path.clone());
    alias.disk_guard_deadline_ms = Some(250);
    let mut opened_count = 0;
    let error =
        open_effective_backends_with(&config, &[first, alias], Some(2), |cfg, readers, wal| {
            opened_count += 1;
            let opened = open_backend_with_policies(
                cfg,
                readers,
                wal,
                cfg.resolve_disk_guard(&config.disk_guard_environment)?,
                config.volume_lock_dir.as_deref(),
            )?;
            Ok(opened)
        })
        .err()
        .expect("cached alias must reject conflicting disk policy");
    // The post-snapshot alias identity checks must not be bypassed merely to
    // reuse a newly-created hard link with a conflicting disk policy.
    assert!(matches!(
        error.downcast_ref::<khive_runtime::ConfigError>(),
        Some(khive_runtime::ConfigError::DiskGuardAliasConflict { .. })
    ));
    assert_eq!(opened_count, 1);
}

#[test]
fn named_disk_policy_opener_forwards_each_policy_and_memory_disables_it() {
    let dir = tempfile::tempdir().unwrap();
    let config = runtime(dir.path().join("main.db"), dir.path().join("locks"));
    let mut first = backend("main", dir.path().join("main.db"));
    let mut second = backend("archive", dir.path().join("archive.db"));
    first.disk_reserve_bytes = Some(123);
    first.disk_guard_deadline_ms = Some(250);
    second.disk_reserve_bytes = Some(456);
    second.disk_guard_deadline_ms = Some(350);
    let opened = open_effective_backends_with(
        &config,
        &[first.clone(), second.clone()],
        Some(2),
        |cfg, readers, wal| {
            open_backend_with_policies(
                cfg,
                readers,
                wal,
                cfg.resolve_disk_guard(&config.disk_guard_environment)?,
                config.volume_lock_dir.as_deref(),
            )
        },
    )
    .unwrap();
    for (name, numbers) in [("main", (123, 250)), ("archive", (456, 350))] {
        assert_eq!(
            disk_guard_numbers(opened[name].pool().effective_disk_guard_config()),
            Some(numbers)
        );
    }
    first.kind = BackendKind::Memory;
    assert!(first
        .resolve_disk_guard(&config.disk_guard_environment)
        .is_err());
    first.disk_reserve_bytes = Some(0);
    assert!(first
        .resolve_disk_guard(&config.disk_guard_environment)
        .unwrap()
        .is_none());
    let forced = effective_backend_configs(&[second], true);
    assert!(forced[0]
        .resolve_disk_guard(&config.disk_guard_environment)
        .unwrap()
        .is_none());
}

#[test]
fn explicit_implicit_policy_fingerprint_ignores_provenance_and_exempts_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime(dir.path().join("main.db"), dir.path().join("locks"));
    config.disk_guard_config = Some(EffectiveDiskGuardConfig {
        reserve_bytes: 123,
        guard_deadline_ms: 250,
        ..EffectiveDiskGuardConfig::default()
    });
    let id = crate::server::compute_config_id(&config, None);
    config.disk_guard_config.as_mut().unwrap().reserve_source = DiskGuardConfigSource::Backend;
    assert_eq!(id, crate::server::compute_config_id(&config, None));
    let readonly = crate::server::compute_config_id_with_storage_mode(&config, None, true);
    config.disk_guard_config.as_mut().unwrap().reserve_bytes = 456;
    assert_ne!(id, crate::server::compute_config_id(&config, None));
    assert_eq!(
        readonly,
        crate::server::compute_config_id_with_storage_mode(&config, None, true)
    );
}

// MUST-FAIL: folding only the numbers of backends that name a reserve or a
// deadline (or ignoring the environment-resolved ones) leaves one of the
// `assert_ne!` arms equal to the baseline.
#[test]
fn mixed_disk_policy_fingerprint_follows_every_effective_number() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime(dir.path().join("main.db"), dir.path().join("locks"));
    config.disk_guard_environment = environment("123", "250");
    let main = backend("main", dir.path().join("main.db"));
    let mut archive = backend("archive", dir.path().join("archive.db"));
    archive.disk_reserve_bytes = Some(456);
    let mut notes = backend("notes", dir.path().join("notes.db"));
    notes.disk_guard_deadline_ms = Some(350);
    let topology = KhiveConfig {
        backends: vec![main, archive, notes],
        ..KhiveConfig::default()
    };
    let id_of = |config: &RuntimeConfig, topology: &KhiveConfig| {
        crate::server::compute_config_id(config, Some(topology))
    };
    let baseline = id_of(&config, &topology);
    let rows = r#"[["archive",456,250],["main",123,250],["notes",123,350]]"#;
    assert!(
        baseline.contains(&format!(";sqlite_disk_guard={rows}")),
        "{baseline}"
    );

    // Any one effective number moving changes the identity.
    let mut changed_reserve = config.clone();
    changed_reserve.disk_guard_environment = environment("124", "250");
    assert_ne!(baseline, id_of(&changed_reserve, &topology));
    let mut changed_deadline = config.clone();
    changed_deadline.disk_guard_environment = environment("123", "251");
    assert_ne!(baseline, id_of(&changed_deadline, &topology));
    let mut archive_reserve = topology.clone();
    archive_reserve.backends[1].disk_reserve_bytes = Some(457);
    assert_ne!(baseline, id_of(&config, &archive_reserve));
    let mut notes_deadline = topology.clone();
    notes_deadline.backends[2].disk_guard_deadline_ms = Some(351);
    assert_ne!(baseline, id_of(&config, &notes_deadline));

    // An equivalent input moving between TOML and the environment does not.
    let mut explicit = topology.clone();
    explicit.backends[0].disk_reserve_bytes = Some(123);
    explicit.backends[0].disk_guard_deadline_ms = Some(250);
    explicit.backends[1].disk_guard_deadline_ms = Some(250);
    explicit.backends[2].disk_reserve_bytes = Some(123);
    assert_eq!(baseline, id_of(&config, &explicit));
    let mut legacy = config.clone();
    legacy.disk_guard_environment = DiskGuardEnvironment {
        reserve: None,
        deadline: Some("250".into()),
        legacy_reserve: Some("123".into()),
    };
    assert_eq!(baseline, id_of(&legacy, &topology));
    let mut reversed = topology.clone();
    reversed.backends.reverse();
    assert_eq!(baseline, id_of(&config, &reversed));
}

// MUST-FAIL: folding the implicit main only when no backend is declared leaves
// the first two `assert_ne!` arms equal to the baseline.
#[test]
fn implicit_main_policy_still_folds_when_declared_backends_omit_main() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime(dir.path().join("main.db"), dir.path().join("locks"));
    config.disk_guard_environment = environment("123", "250");
    let mut archive = backend("archive", dir.path().join("archive.db"));
    archive.disk_reserve_bytes = Some(456);
    archive.disk_guard_deadline_ms = Some(350);
    let topology = KhiveConfig {
        backends: vec![archive],
        ..KhiveConfig::default()
    };
    let baseline = crate::server::compute_config_id(&config, Some(&topology));
    let rows = r#"[["archive",456,350],["main",123,250]]"#;
    assert!(
        baseline.contains(&format!(";sqlite_disk_guard={rows}")),
        "{baseline}"
    );

    let mut changed_environment = config.clone();
    changed_environment.disk_guard_environment = environment("124", "250");
    assert_ne!(
        baseline,
        crate::server::compute_config_id(&changed_environment, Some(&topology))
    );
    let mut programmatic = config.clone();
    programmatic.disk_guard_config = Some(EffectiveDiskGuardConfig {
        reserve_bytes: 123,
        guard_deadline_ms: 251,
        ..EffectiveDiskGuardConfig::default()
    });
    assert_ne!(
        baseline,
        crate::server::compute_config_id(&programmatic, Some(&topology))
    );
    // The same numbers from a different source are the same policy.
    programmatic.disk_guard_config = Some(EffectiveDiskGuardConfig {
        reserve_bytes: 123,
        guard_deadline_ms: 250,
        reserve_source: DiskGuardConfigSource::Backend,
        ..EffectiveDiskGuardConfig::default()
    });
    assert_eq!(
        baseline,
        crate::server::compute_config_id(&programmatic, Some(&topology))
    );

    // Read-only storage and an in-memory main have no implicit writer row.
    let read_only =
        crate::server::compute_config_id_with_storage_mode(&config, Some(&topology), true);
    let archive_only = r#"[["archive",456,350]]"#;
    assert!(
        read_only.contains(&format!(";sqlite_disk_guard={archive_only}")),
        "{read_only}"
    );
    let mut memory = config.clone();
    memory.db_path = None;
    let memory_id = crate::server::compute_config_id(&memory, Some(&topology));
    assert!(
        memory_id.contains(&format!(";sqlite_disk_guard={archive_only}")),
        "{memory_id}"
    );
}

#[test]
fn invalid_disk_environment_folds_to_a_deterministic_marker() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime(dir.path().join("main.db"), dir.path().join("locks"));
    config.disk_guard_environment = environment("not-a-number", "250");
    let id = crate::server::compute_config_id(&config, None);
    assert!(id.ends_with(";sqlite_disk_guard=invalid"), "{id}");
    assert_eq!(id, crate::server::compute_config_id(&config, None));
}
